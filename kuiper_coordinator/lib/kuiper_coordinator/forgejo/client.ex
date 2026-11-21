defmodule KuiperCoordinator.Forgejo.Client do
  @moduledoc """
  Forgejo client for one-job runner provisioning, ported from kuiper-forge/src/forgejo.rs
  (parity FJ1-FJ7). Talks to `{url}/api/v1` with `Authorization: token <t>`; needs Forgejo 15+
  (runner jobs endpoint, job handles).

  Scopes are `org`, `owner/repo`, `@user` (the token's own user) or `*` (the instance). Forgejo
  only runs one-job runners: each job gets its own ephemeral runner, so there are no
  registration tokens and no fixed-capacity pools.
  """
  @behaviour KuiperCoordinator.Forge
  require Logger

  alias KuiperCoordinator.Forge.{QueuedJob, RunnerScope}

  @runner_page_size 50
  @max_runner_pages 100
  @timeout_ms 30_000

  defstruct [:base_url, :token, :scopes, :req, :repo_names]

  @type t :: %__MODULE__{}

  @doc """
  Options: `:url` (http or https), the token from `:token_env` (an env var name), else
  `:token_path` (a file), else `:token`; `:scopes` (config strings, at least one);
  `:req_options` (extra `Req` options, e.g. a test plug); `:cache` (an Agent pid or name for
  the repo-name cache; the fleet passes a supervised one).
  """
  @spec new(keyword()) :: {:ok, t()} | {:error, String.t()}
  def new(opts) do
    base_url = opts |> Keyword.get(:url, "") |> String.trim_trailing("/")

    with :ok <- check_url(base_url),
         {:ok, token} <- token(opts),
         {:ok, scopes} <- scopes(Keyword.get(opts, :scopes, [])) do
      req =
        Req.new(
          base_url: base_url <> "/api/v1",
          headers: [
            {"authorization", "token " <> token},
            {"accept", "application/json"},
            {"user-agent", "kuiper-forge"}
          ],
          receive_timeout: @timeout_ms
        )
        |> Req.merge(Keyword.get(opts, :req_options, []))

      # The repo-name cache must outlive whoever calls new/1 (a config load, a Task): use the
      # fleet's supervised cache when given, else an unlinked Agent.
      names =
        case Keyword.fetch(opts, :cache) do
          {:ok, cache} ->
            cache

          :error ->
            {:ok, pid} = Agent.start(fn -> %{} end)
            pid
        end

      {:ok,
       %__MODULE__{base_url: base_url, token: token, scopes: scopes, req: req, repo_names: names}}
    end
  end

  defp check_url(url) do
    case URI.parse(url) do
      %URI{scheme: s, host: h} when s in ["http", "https"] and is_binary(h) and h != "" -> :ok
      _ -> {:error, "Forgejo url must be http(s), got #{inspect(url)}"}
    end
  end

  defp token(opts) do
    token =
      cond do
        env = opts[:token_env] -> System.get_env(env)
        path = opts[:token_path] -> path |> File.read() |> then(&elem_or_nil/1)
        true -> opts[:token]
      end

    case token && String.trim(token) do
      t when is_binary(t) and t != "" -> {:ok, t}
      _ -> {:error, "Forgejo token is empty (set token_env, token_path or token)"}
    end
  end

  defp elem_or_nil({:ok, v}), do: v
  defp elem_or_nil(_), do: nil

  defp scopes([]), do: {:error, "a Forgejo forge needs at least one runner scope"}

  defp scopes(list) do
    Enum.reduce_while(list, {:ok, []}, fn s, {:ok, acc} ->
      case RunnerScope.parse(s) do
        {:ok, scope} -> {:cont, {:ok, acc ++ [scope]}}
        err -> {:halt, err}
      end
    end)
  end

  @doc "The API path prefix for a scope: `orgs/{org}`, `repos/{o}/{r}`, `user` or `admin`."
  def scope_path({:org, name}), do: "orgs/#{name}"
  def scope_path({:repo, owner, repo}), do: "repos/#{owner}/#{repo}"
  def scope_path({:user, _}), do: "user"
  def scope_path(:instance), do: "admin"

  @doc """
  Startup check (FJ7): a `@user` scope is the token's own user; org/user/instance scopes can
  read repositories (they name each job's repo by id); every scope lists jobs and may manage
  runners (deleting runner 0, which can't exist, gives 404 with write access, 403 without).
  """
  @spec validate(t()) :: :ok | {:error, String.t()}
  def validate(%__MODULE__{} = c) do
    with :ok <- validate_user(c),
         :ok <- validate_repo_read(c) do
      Enum.reduce_while(c.scopes, :ok, fn scope, :ok ->
        case validate_scope(c, scope) do
          :ok -> {:cont, :ok}
          err -> {:halt, err}
        end
      end)
    end
  end

  defp validate_user(c) do
    case Enum.find_value(c.scopes, fn s -> match?({:user, _}, s) && elem(s, 1) end) do
      nil ->
        :ok

      name ->
        case get(c, "user", "getting the token's user") do
          {:ok, %{"login" => login}} when is_binary(login) ->
            if String.downcase(login) == String.downcase(name),
              do: :ok,
              else:
                {:error,
                 "Scope '@#{name}' must be the token's own user, but the token belongs to '#{login}'"}

          {:ok, _} ->
            {:error, "Failed to parse Forgejo user response"}

          {:error, msg} ->
            {:error, "A user scope needs a token with write:user: " <> msg}
        end
    end
  end

  defp validate_repo_read(c) do
    if Enum.any?(c.scopes, &(not match?({:repo, _, _}, &1))) do
      case raw(c, :get, "repositories/0") do
        {:ok, %{status: s} = resp} when s in [401, 403] ->
          {:error,
           "Org, user and instance scopes need a token with read:repository, to name the " <>
             "repositories jobs come from: " <> error_for("checking repository access", resp)}

        {:ok, _} ->
          :ok

        {:error, e} ->
          {:error, transport("checking repository access", e)}
      end
    else
      :ok
    end
  end

  defp validate_scope(c, scope) do
    path = scope_path(scope) <> "/actions/runners/0"

    with {:list, {:ok, _}} <- {:list, list_scope_jobs(c, scope)},
         {:ok, %{status: s}} when s in [400, 404] or s in 200..299 <- raw(c, :delete, path) do
      :ok
    else
      {:list, {:error, msg}} ->
        {:error, "Token can't list jobs in #{RunnerScope.to_string(scope)}: " <> msg}

      {:ok, resp} ->
        {:error,
         "Token can't manage runners in #{RunnerScope.to_string(scope)}: " <>
           error_for("checking runner access", resp)}

      {:error, e} ->
        {:error, transport("checking runner access", e)}
    end
  end

  @impl KuiperCoordinator.Forge
  def runner_kind(_c), do: "forgejo"

  @impl KuiperCoordinator.Forge
  def scope_url(c, {:org, name}), do: "#{c.base_url}/#{name}"
  def scope_url(c, {:user, name}), do: "#{c.base_url}/#{name}"
  def scope_url(c, {:repo, owner, repo}), do: "#{c.base_url}/#{owner}/#{repo}"
  def scope_url(c, :instance), do: c.base_url

  @doc "Always an error: Forgejo forges only run one-job runners, not fixed-capacity pools (FJ2)."
  @impl KuiperCoordinator.Forge
  def registration_token(_c, _scope),
    do:
      {:error, "Forgejo forges only run one-job runners (polling mode), not fixed-capacity pools"}

  @doc "Registers an ephemeral runner for the job attempt `handle` (FJ2)."
  @impl KuiperCoordinator.Forge
  def create_job_runner(c, scope, runner_name, _labels, _runner_group, handle) do
    path = scope_path(scope) <> "/actions/runners"

    body = %{
      name: runner_name,
      description: "kuiper-forge one-job runner for job #{handle}",
      ephemeral: true
    }

    case request(c, :post, path, "registering runner", json: body) do
      {:ok, %{"uuid" => uuid, "token" => token} = r} when is_binary(uuid) and is_binary(token) ->
        Logger.info(
          "Registered Forgejo runner #{inspect(runner_name)} (id #{r["id"]}, uuid #{uuid}) " <>
            "in #{RunnerScope.to_string(scope)} for job #{handle}"
        )

        {:ok, {:forgejo, %{instance_url: c.base_url, uuid: uuid, token: token, handle: handle}}}

      {:ok, _} ->
        {:error, "Failed to parse Forgejo runner registration response"}

      err ->
        err
    end
  end

  @doc "Deletes every runner with this exact name; one already gone is fine (FJ4)."
  @impl KuiperCoordinator.Forge
  def remove_runner(c, scope, runner_name) do
    with {:ok, runners} <- list_runners(c, scope) do
      runners
      |> Enum.filter(&(&1["name"] == runner_name))
      |> Enum.reduce_while(:ok, fn %{"id" => id}, :ok ->
        path = scope_path(scope) <> "/actions/runners/#{id}"

        case raw(c, :delete, path) do
          {:ok, %{status: 404}} ->
            {:cont, :ok}

          {:ok, %{status: s}} when s in 200..299 ->
            Logger.info("Removed Forgejo runner #{inspect(runner_name)} (id #{id})")
            {:cont, :ok}

          {:ok, resp} ->
            {:halt, {:error, error_for("deleting runner", resp)}}

          {:error, e} ->
            {:halt, {:error, transport("deleting runner", e)}}
        end
      end)
    end
  end

  @doc """
  Never removes anything (FJ4): one-job runners are bound to a job attempt, and Forgejo's
  runner status reflects check-ins, not whether a job runs. Registered -> `:busy`, else
  `:not_registered`.
  """
  @spec remove_idle_runner(t(), RunnerScope.t(), String.t()) ::
          {:ok, :busy | :not_registered} | {:error, term()}
  def remove_idle_runner(c, scope, runner_name) do
    with {:ok, runners} <- list_runners(c, scope) do
      if Enum.any?(runners, &(&1["name"] == runner_name)),
        do: {:ok, :busy},
        else: {:ok, :not_registered}
    end
  end

  @doc """
  Every runner registered directly in a scope. Pages until an EMPTY page: the server caps
  page size at its MAX_RESPONSE_ITEMS, which can be below what we ask for, so a short page
  isn't the end (FJ3, Rust a9a1f17). Stops after #{@max_runner_pages} pages.
  """
  def list_runners(c, scope), do: list_runners(c, scope, 1, [])

  defp list_runners(_c, scope, page, acc) when page > @max_runner_pages do
    Logger.warning(
      "Stopped after #{@max_runner_pages} pages of runners in " <>
        "#{RunnerScope.to_string(scope)} - listing may be incomplete"
    )

    {:ok, acc}
  end

  defp list_runners(c, scope, page, acc) do
    path = scope_path(scope) <> "/actions/runners"

    case request(c, :get, path, "listing runners", params: [page: page, limit: @runner_page_size]) do
      {:ok, list} when list in [nil, []] -> {:ok, acc}
      {:ok, list} when is_list(list) -> list_runners(c, scope, page + 1, acc ++ list)
      {:ok, _} -> {:error, "Failed to parse runners list from #{path}"}
      err -> err
    end
  end

  @doc """
  Every waiting job across the scopes (FJ5). Jobs without a handle (Forgejo < 15) are
  skipped; overlapping scopes are deduped by handle, first scope wins. A failing scope is
  skipped; all failing is an error.
  """
  @impl KuiperCoordinator.Forge
  def list_queued_jobs(c) do
    {jobs, _seen, failed} =
      Enum.reduce(c.scopes, {[], MapSet.new(), 0}, fn scope, {jobs, seen, failed} ->
        case list_scope_jobs(c, scope) do
          {:ok, list} ->
            {new, seen} = queued_in_scope(c, scope, list, seen)
            {jobs ++ new, seen, failed}

          {:error, msg} ->
            Logger.warning(
              "Queued-job scan: skipping Forgejo scope #{RunnerScope.to_string(scope)}: #{msg}"
            )

            {jobs, seen, failed + 1}
        end
      end)

    if failed == length(c.scopes),
      do: {:error, "Couldn't list jobs in any of #{failed} Forgejo scope(s)"},
      else: {:ok, jobs}
  end

  defp queued_in_scope(c, scope, list, seen) do
    Enum.reduce(list, {[], seen}, fn job, {acc, seen} ->
      handle = job["handle"] || ""

      cond do
        job["status"] != "waiting" ->
          {acc, seen}

        handle == "" ->
          Logger.warning(
            "Forgejo job #{job["id"]} (#{inspect(job["name"])}) has no handle - " <>
              "needs Forgejo 15 or newer, skipping"
          )

          {acc, seen}

        MapSet.member?(seen, handle) ->
          {acc, seen}

        true ->
          queued = %QueuedJob{
            id: handle,
            labels: job["runs_on"] || [],
            runner_scope: scope,
            repository: repository(c, scope, job),
            # the runner jobs API has no created time
            created_at: nil
          }

          {acc ++ [queued], MapSet.put(seen, handle)}
      end
    end)
  end

  defp repository(_c, {:repo, owner, repo}, _job), do: owner <> "/" <> repo

  defp repository(c, _scope, job) do
    case repo_name(c, job["repo_id"]) do
      {:ok, name} ->
        name

      {:error, msg} ->
        Logger.warning(
          "Couldn't look up repository #{job["repo_id"]} for Forgejo job #{job["handle"]}: #{msg}"
        )

        nil
    end
  end

  defp repo_name(c, id) do
    case Agent.get(c.repo_names, &Map.get(&1, id)) do
      nil ->
        case request(c, :get, "repositories/#{id}", "getting repository") do
          {:ok, %{"full_name" => name}} when is_binary(name) ->
            Agent.update(c.repo_names, &Map.put(&1, id, name))
            {:ok, name}

          {:ok, _} ->
            {:error, "Failed to parse repository #{id}"}

          err ->
            err
        end

      name ->
        {:ok, name}
    end
  end

  @doc """
  A job's status in GitHub's terms (`waiting` -> `queued`, `running` -> `in_progress`), or nil
  when its handle is no longer listed (finished). A list error is an error, not nil (FJ6).
  """
  @impl KuiperCoordinator.Forge
  def job_status(c, repo, handle) do
    with {:ok, scope} <- scope_for_repo(c, repo),
         {:ok, jobs} <- list_scope_jobs(c, scope) do
      {:ok,
       Enum.find_value(jobs, fn job ->
         job["handle"] == handle && github_status(job["status"])
       end)}
    end
  end

  @doc """
  A job's status in one scope's job list (#48: runners without a repository, org/user/instance
  scopes), in GitHub's terms; nil when the handle isn't listed.
  """
  @impl KuiperCoordinator.Forge
  def scope_job_status(c, scope, handle) do
    with {:ok, jobs} <- list_scope_jobs(c, scope) do
      {:ok,
       Enum.find_value(jobs, fn job ->
         job["handle"] == handle && github_status(job["status"])
       end)}
    end
  end

  defp github_status("waiting"), do: "queued"
  defp github_status("running"), do: "in_progress"
  defp github_status(other), do: other

  @doc """
  The scope to ask about a repository's jobs: a configured org or user owning it
  (case-insensitive), else the instance if configured, else the repo itself (FJ6; this order
  differs from the webhook's on purpose, as in Rust).
  """
  def scope_for_repo(c, repo) do
    case String.split(repo, "/", parts: 2) do
      [owner, name] when owner != "" and name != "" ->
        owning =
          Enum.find(c.scopes, fn
            {kind, n} when kind in [:org, :user] -> String.downcase(n) == String.downcase(owner)
            _ -> false
          end)

        cond do
          owning -> {:ok, owning}
          :instance in c.scopes -> {:ok, :instance}
          true -> {:ok, {:repo, owner, name}}
        end

      _ ->
        {:error, "Invalid repository '#{repo}': expected 'owner/repo'"}
    end
  end

  # Every waiting and running job in a scope (unpaged). Forgejo answers `null` for none.
  defp list_scope_jobs(c, scope) do
    case request(c, :get, scope_path(scope) <> "/actions/runners/jobs", "listing jobs") do
      {:ok, nil} -> {:ok, []}
      {:ok, list} when is_list(list) -> {:ok, list}
      {:ok, _} -> {:error, "Failed to parse jobs list"}
      err -> err
    end
  end

  defp get(c, path, what), do: request(c, :get, path, what)

  defp request(c, method, path, what, opts \\ []) do
    case raw(c, method, path, opts) do
      {:ok, %{status: s, body: body}} when s in 200..299 ->
        case decode(body) do
          {:ok, data} -> {:ok, data}
          :error -> {:error, "Failed to parse #{what} response: not JSON"}
        end

      {:ok, resp} ->
        {:error, error_for(what, resp)}

      {:error, e} ->
        {:error, transport(what, e)}
    end
  end

  # A 2xx that isn't JSON (a proxy's HTML page) is an error, as in Rust ("Failed to parse ...").
  defp decode(""), do: {:ok, nil}

  defp decode(body) when is_binary(body) do
    case Jason.decode(body) do
      {:ok, data} -> {:ok, data}
      {:error, _} -> :error
    end
  end

  defp decode(body), do: {:ok, body}

  defp raw(c, method, path, opts \\ []) do
    Req.request(c.req, [method: method, url: "/" <> path] ++ opts)
  end

  defp error_for(what, %{status: status, body: body}) do
    hint =
      case status do
        401 ->
          " (is the token valid?)"

        403 ->
          " (the token's user must own the scope: org owner with write:organization, " <>
            "or repo owner with write:repository)"

        _ ->
          ""
      end

    body = if is_binary(body), do: String.trim(body), else: Jason.encode!(body)
    Logger.warning("Forgejo API error (#{what}): #{status} #{body}")
    "Forgejo API error #{what} (#{status})#{hint}: #{body}"
  end

  defp transport(what, e), do: "Forgejo API request failed (#{what}): #{Exception.message(e)}"
end
