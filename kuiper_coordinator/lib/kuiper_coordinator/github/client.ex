defmodule KuiperCoordinator.GitHub.Client do
  @moduledoc """
  GitHub App client for runner provisioning, ported from kuiper-forge/src/github.rs
  (parity G1-G8). Auth with an App JWT, an installation per account, installation tokens
  cached until 5 minutes before they expire.
  """
  @behaviour KuiperCoordinator.Forge
  require Logger

  alias KuiperCoordinator.Forge.{QueuedJob, RunnerScope}
  alias KuiperCoordinator.GitHub.{Client.Cache, JWT, Pages}

  @api "https://api.github.com"
  @token_margin_secs 5 * 60
  @default_runner_group 1
  @default_scan_lookback_hours 168
  @scan_concurrency 20

  defstruct [
    :app_id,
    :private_key,
    :cache,
    :req,
    scan_lookback_hours: @default_scan_lookback_hours,
    now: &DateTime.utc_now/0
  ]

  @type t :: %__MODULE__{}
  @type scope :: RunnerScope.t()

  @doc """
  Options: `:app_id`, `:private_key` (PEM; escaped `\\n` allowed), `:cache` (a
  `#{inspect(Cache)}` pid or name), `:req_options` (extra `Req` options, e.g. a test plug),
  `:now` (a clock function, for tests), `:scan_lookback_hours` (recovery scan only looks at
  repos pushed this recently; default #{@default_scan_lookback_hours}, `0` scans every repo).
  """
  def new(opts) do
    req =
      Req.new(
        base_url: Keyword.get(opts, :base_url, @api),
        headers: [
          {"accept", "application/vnd.github+json"},
          {"x-github-api-version", "2022-11-28"},
          {"user-agent", "kuiper-forge"}
        ]
      )
      |> Req.merge(Keyword.get(opts, :req_options, []))

    %__MODULE__{
      app_id: Keyword.fetch!(opts, :app_id),
      private_key: Keyword.fetch!(opts, :private_key),
      cache: Keyword.fetch!(opts, :cache),
      req: req,
      scan_lookback_hours: Keyword.get(opts, :scan_lookback_hours, @default_scan_lookback_hours),
      now: Keyword.get(opts, :now, &DateTime.utc_now/0)
    }
  end

  @doc "Startup check (G1): the key signs a JWT and the App has at least one installation."
  @spec validate(t()) :: {:ok, [String.t()]} | {:error, term()}
  def validate(%__MODULE__{} = c) do
    with {:ok, _} <- jwt(c),
         :ok <- discover_installations(c) do
      case Cache.installations(c.cache) do
        [] -> {:error, :no_installations}
        accounts -> {:ok, accounts}
      end
    end
  end

  @doc "A runner registration token for the scope (fixed-capacity pools)."
  @impl KuiperCoordinator.Forge
  @spec registration_token(t(), scope()) :: {:ok, String.t()} | {:error, term()}
  def registration_token(c, scope) do
    with {:ok, token} <- scope_token(c, scope),
         {:ok, %{"token" => reg}} <-
           request(c, token, :post, RunnerScope.registration_token_path(scope), [201]) do
      {:ok, reg}
    end
  end

  @doc "A JIT runner config pre-bound to one job (webhook mode, G6)."
  @spec jit_config(t(), scope(), String.t(), [String.t()], pos_integer()) ::
          {:ok, String.t()} | {:error, term()}
  def jit_config(c, scope, runner_name, labels, runner_group_id) do
    body = %{
      name: runner_name,
      runner_group_id: runner_group_id,
      labels: labels,
      work_folder: "_work"
    }

    with {:ok, token} <- scope_token(c, scope),
         {:ok, %{"encoded_jit_config" => jit} = resp} <-
           request(c, token, :post, RunnerScope.jitconfig_path(scope), [201], json: body) do
      Logger.info(
        "Generated JIT config for runner '#{runner_name}' (GitHub runner ID: #{get_in(resp, ["runner", "id"])})"
      )

      {:ok, jit}
    end
  end

  @doc """
  The runner group id for a configured `runner_group`: nil is the Default group (1), an integer
  or numeric string is an id, and a name is looked up in the org's runner groups. An unknown
  name is `{:error, {:unknown_runner_group, name}}`, never a silent fallback to group 1 (D9).
  """
  @spec resolve_runner_group(t(), scope(), nil | integer() | String.t()) ::
          {:ok, pos_integer()} | {:error, term()}
  def resolve_runner_group(_c, _scope, nil), do: {:ok, @default_runner_group}
  def resolve_runner_group(_c, _scope, id) when is_integer(id) and id > 0, do: {:ok, id}

  def resolve_runner_group(c, scope, name) when is_binary(name) do
    case Integer.parse(name) do
      {id, ""} when id > 0 ->
        {:ok, id}

      _ ->
        with {:ok, token} <- scope_token(c, scope),
             org = RunnerScope.account(scope),
             {:ok, groups} <-
               Pages.collect(fn page ->
                 with {:ok, %{"runner_groups" => gs}} <-
                        request(c, token, :get, "/orgs/#{org}/actions/runner-groups", [200],
                          params: [per_page: Pages.page_size(), page: page]
                        ),
                      do: {:ok, gs}
               end) do
          case Enum.find(groups, &(&1["name"] == name)) do
            %{"id" => id} -> {:ok, id}
            nil -> {:error, {:unknown_runner_group, name}}
          end
        end
    end
  end

  @doc """
  Unregisters a runner only if it isn't running a job (G7): `:removed`, `:busy` (GitHub says
  busy, or refuses the delete with 422), or `:not_registered`.
  """
  @impl KuiperCoordinator.Forge
  @spec remove_idle_runner(t(), scope(), String.t()) ::
          {:ok, :removed | :busy | :not_registered} | {:error, term()}
  def remove_idle_runner(c, scope, name) do
    with {:ok, token} <- scope_token(c, scope),
         {:ok, runner} <- find_runner(c, token, scope, name) do
      cond do
        runner == nil ->
          {:ok, :not_registered}

        runner["busy"] ->
          {:ok, :busy}

        true ->
          case delete_runner(c, token, scope, runner["id"]) do
            :ok -> {:ok, :removed}
            {:error, {:runner_busy, _}} -> {:ok, :busy}
            err -> err
          end
      end
    end
  end

  @impl KuiperCoordinator.Forge
  def runner_kind(_c), do: "github"

  @doc "The scope's URL the runner registers against (forge.rs scope_url)."
  @impl KuiperCoordinator.Forge
  def scope_url(_c, scope), do: RunnerScope.github_url(scope)

  @doc "Removes a runner by name; absent is fine, busy (422) is an error."
  @impl KuiperCoordinator.Forge
  @spec remove_runner(t(), scope(), String.t()) :: :ok | {:error, term()}
  def remove_runner(c, scope, name) do
    with {:ok, token} <- scope_token(c, scope),
         {:ok, runner} <- find_runner(c, token, scope, name) do
      case runner do
        nil ->
          Logger.info("Runner '#{name}' not found (may have already been removed)")
          :ok

        %{"id" => id} ->
          delete_runner(c, token, scope, id)
      end
    end
  end

  @doc """
  A single-use JIT runner for a job (forge.rs create_job_runner): resolves the job's runner
  group (unknown name fails, D9), then generates the JIT config.
  """
  @impl KuiperCoordinator.Forge
  def create_job_runner(c, scope, runner_name, labels, runner_group, _job_id) do
    with {:ok, group_id} <- resolve_runner_group(c, scope, runner_group),
         {:ok, jit} <- jit_config(c, scope, runner_name, labels, group_id) do
      {:ok, {:github_jit, jit}}
    end
  end

  @doc "GitHub jobs are looked up by repository only; JIT runners aren't idle-checked."
  @impl KuiperCoordinator.Forge
  def scope_job_status(_c, _scope, _job_id), do: {:error, :unsupported}

  @doc "A workflow job's status (`queued`, `in_progress`, ...); nil when GitHub returns 404 (G8)."
  @impl KuiperCoordinator.Forge
  @spec job_status(t(), String.t(), integer() | String.t()) ::
          {:ok, String.t() | nil} | {:error, term()}
  def job_status(c, repo, job_id) when is_binary(job_id) do
    case Integer.parse(job_id) do
      {id, ""} -> job_status(c, repo, id)
      _ -> {:error, {:invalid_job_id, job_id}}
    end
  end

  def job_status(c, repo, job_id) do
    case String.split(repo, "/") do
      [owner, _name] ->
        with {:ok, token} <- account_token(c, owner) do
          # Match on the HTTP status first: GitHub's 404 body also has a "status" key ("404").
          case raw(c, token, :get, "/repos/#{repo}/actions/jobs/#{job_id}", []) do
            {:ok, %{status: 404}} ->
              {:ok, nil}

            {:ok, %{status: 200, body: %{"status" => status}}} when is_binary(status) ->
              {:ok, status}

            {:ok, %{status: s, body: body}} ->
              {:error, {:http, s, body}}

            {:error, e} ->
              {:error, e}
          end
        end

      _ ->
        {:error, :invalid_repo}
    end
  end

  @doc """
  Recovery scan (G9, github.rs list_queued_jobs): every `queued` or `waiting` job in the
  queued, in_progress and waiting runs of each installation's repos, org-scoped like the
  webhook. Repos not pushed within `scan_lookback_hours` are skipped (non-push triggers on
  them are missed; `0` disables the filter). A failed installation token, repo page, run or
  job listing is logged and skipped, so the rest of the scan still counts.
  """
  @impl KuiperCoordinator.Forge
  @spec list_queued_jobs(t()) :: {:ok, [QueuedJob.t()]} | {:error, term()}
  def list_queued_jobs(c) do
    Logger.info("Scanning GitHub for queued workflow jobs...")

    with :ok <- discover_installations(c) do
      jobs =
        c.cache
        |> Cache.installation_map()
        |> Enum.flat_map(fn {account, id} -> scan_installation(c, account, id) end)

      Logger.info("Found #{length(jobs)} queued workflow jobs across all installations")
      {:ok, jobs}
    end
  end

  defp scan_installation(c, account, id) do
    case installation_token(c, id) do
      {:ok, token} ->
        repos = installation_repos(c, token, account, 1, [])
        active = Enum.filter(repos, &recent_push?(c, &1))

        if length(active) < length(repos) do
          Logger.debug(
            "Queued-job scan: #{length(repos) - length(active)} of #{length(repos)} repos " <>
              "for #{account} skipped by lookback filter"
          )
        end

        active
        |> Task.async_stream(&queued_jobs_in_repo(c, token, &1),
          max_concurrency: @scan_concurrency,
          timeout: :infinity,
          ordered: true
        )
        |> Enum.flat_map(fn {:ok, jobs} -> jobs end)

      {:error, e} ->
        # Loud: a token failure silently drops a whole installation from the scan.
        Logger.warning(
          "Queued-job scan: failed to get access token for #{account} - skipping installation: " <>
            inspect(e)
        )

        []
    end
  end

  # Like Rust, a failed repo page keeps the repos read so far.
  defp installation_repos(c, token, account, page, acc) do
    case request(c, token, :get, "/installation/repositories", [200],
           params: [per_page: Pages.page_size(), page: page]
         ) do
      {:ok, %{"repositories" => repos}} when is_list(repos) ->
        acc = acc ++ repos

        if length(repos) < Pages.page_size() or page >= 100,
          do: acc,
          else: installation_repos(c, token, account, page + 1, acc)

      other ->
        Logger.warning(
          "Queued-job scan: failed to list repos page #{page} for #{account}: #{inspect(other)}"
        )

        acc
    end
  end

  defp recent_push?(%{scan_lookback_hours: 0}, _repo), do: true

  defp recent_push?(c, repo) do
    cutoff = DateTime.add(c.now.(), -c.scan_lookback_hours * 3600)

    case parse_time(repo["pushed_at"]) do
      nil -> true
      pushed -> DateTime.compare(pushed, cutoff) == :gt
    end
  end

  defp queued_jobs_in_repo(c, token, %{"full_name" => full_name, "owner" => %{"login" => owner}}) do
    # Every page of runs and of each run's jobs: in polling mode this scan is the only job
    # source, so a queued job past the first page would never get a runner.
    for run_status <- ["queued", "in_progress", "waiting"],
        run <-
          list_or_skip(
            c,
            token,
            "/repos/#{full_name}/actions/runs",
            "workflow_runs",
            [status: run_status],
            "#{run_status} runs in #{full_name}"
          ),
        job <-
          list_or_skip(
            c,
            token,
            "/repos/#{full_name}/actions/runs/#{run["id"]}/jobs",
            "jobs",
            [],
            "jobs of run #{run["id"]} in #{full_name}"
          ),
        job["status"] in ["queued", "waiting"] do
      %QueuedJob{
        id: to_string(job["id"]),
        labels: job["labels"] || [],
        runner_scope: {:org, owner},
        repository: full_name,
        created_at: parse_time(job["created_at"])
      }
    end
  end

  defp queued_jobs_in_repo(_c, _token, repo) do
    Logger.warning("Queued-job scan: skipping malformed repo #{inspect(repo["full_name"])}")
    []
  end

  defp list_or_skip(c, token, path, key, params, what) do
    result =
      Pages.collect(fn page ->
        case request(c, token, :get, path, [200],
               params: params ++ [per_page: Pages.page_size(), page: page]
             ) do
          {:ok, %{^key => items}} when is_list(items) -> {:ok, items}
          {:ok, body} -> {:error, {:malformed, body}}
          {:error, e} -> {:error, e}
        end
      end)

    case result do
      {:ok, items} ->
        items

      {:error, e} ->
        Logger.warning("Queued-job scan: failed to list #{what}: #{inspect(e)}")
        []
    end
  end

  defp parse_time(s) when is_binary(s) do
    case DateTime.from_iso8601(s) do
      {:ok, t, _} -> t
      _ -> nil
    end
  end

  defp parse_time(_), do: nil

  # -- internals --

  defp find_runner(c, token, scope, name) do
    with {:ok, %{"runners" => runners}} <-
           request(c, token, :get, RunnerScope.runners_path(scope), [200], params: [name: name]) do
      {:ok, Enum.find(runners, &(&1["name"] == name))}
    end
  end

  defp delete_runner(c, token, scope, id) do
    case raw(c, token, :delete, RunnerScope.runner_path(scope, id), []) do
      {:ok, %{status: s}} when s in 200..299 -> :ok
      {:ok, %{status: 422}} -> {:error, {:runner_busy, id}}
      {:ok, %{status: s, body: b}} -> {:error, {:http, s, b}}
      {:error, e} -> {:error, e}
    end
  end

  defp scope_token(c, scope) do
    if RunnerScope.github?(scope) do
      account_token(c, RunnerScope.account(scope))
    else
      {:error, {:unsupported_scope, scope}}
    end
  end

  defp account_token(c, account) do
    with {:ok, id} <- installation_id(c, account), do: installation_token(c, id)
  end

  defp installation_id(c, account) do
    key = String.downcase(account)

    case Cache.installation(c.cache, key) do
      nil ->
        with :ok <- discover_installations(c) do
          case Cache.installation(c.cache, key) do
            nil -> {:error, {:not_installed, account}}
            id -> {:ok, id}
          end
        end

      id ->
        {:ok, id}
    end
  end

  defp discover_installations(c) do
    with {:ok, jwt} <- jwt(c),
         {:ok, installs} <-
           Pages.collect(fn page ->
             request(c, jwt, :get, "/app/installations", [200],
               params: [per_page: Pages.page_size(), page: page]
             )
           end) do
      Cache.put_installations(
        c.cache,
        Map.new(installs, &{String.downcase(&1["account"]["login"]), &1["id"]})
      )
    end
  end

  defp installation_token(c, id) do
    now = c.now.()

    case Cache.token(c.cache, id) do
      {token, expires_at} ->
        if DateTime.diff(expires_at, now) > @token_margin_secs,
          do: {:ok, token},
          else: fetch_installation_token(c, id)

      nil ->
        fetch_installation_token(c, id)
    end
  end

  defp fetch_installation_token(c, id) do
    with {:ok, jwt} <- jwt(c),
         {:ok, %{"token" => token, "expires_at" => exp}} <-
           request(c, jwt, :post, "/app/installations/#{id}/access_tokens", [201]),
         true <- is_binary(token) and is_binary(exp),
         {:ok, expires_at, _} <- DateTime.from_iso8601(exp) do
      Cache.put_token(c.cache, id, token, expires_at)
      {:ok, token}
    else
      {:error, _} = err -> err
      other -> {:error, {:malformed_token_response, other}}
    end
  end

  defp jwt(c), do: JWT.app_jwt(c.app_id, c.private_key, DateTime.to_unix(c.now.()))

  defp request(c, token, method, url, ok_statuses, opts \\ []) do
    case raw(c, token, method, url, opts) do
      {:ok, %{status: s, body: body}} ->
        if s in ok_statuses, do: {:ok, body}, else: {:error, {:http, s, body}}

      {:error, e} ->
        {:error, e}
    end
  end

  defp raw(c, token, method, url, opts) do
    Req.request(
      c.req,
      [method: method, url: url, auth: {:bearer, token}] ++ opts
    )
  end
end
