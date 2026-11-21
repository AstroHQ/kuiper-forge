defmodule KuiperCoordinator.GitHub.Client do
  @moduledoc """
  GitHub App client for runner provisioning, ported from kuiper-forge/src/github.rs
  (parity G1-G8). Auth with an App JWT, an installation per account, installation tokens
  cached until 5 minutes before they expire.
  """
  require Logger

  alias KuiperCoordinator.Forge.RunnerScope
  alias KuiperCoordinator.GitHub.{Client.Cache, JWT, Pages}

  @api "https://api.github.com"
  @token_margin_secs 5 * 60
  @default_runner_group 1

  defstruct [:app_id, :private_key, :cache, :req, now: &DateTime.utc_now/0]

  @type t :: %__MODULE__{}
  @type scope :: RunnerScope.t()

  @doc """
  Options: `:app_id`, `:private_key` (PEM; escaped `\\n` allowed), `:cache` (a
  `#{inspect(Cache)}` pid or name), `:req_options` (extra `Req` options, e.g. a test plug),
  `:now` (a clock function, for tests).
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

  @doc "Removes a runner by name; absent is fine, busy (422) is an error."
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

  @doc "A workflow job's status (`queued`, `in_progress`, ...); nil when GitHub returns 404 (G8)."
  @spec job_status(t(), String.t(), integer()) :: {:ok, String.t() | nil} | {:error, term()}
  def job_status(c, repo, job_id) do
    case String.split(repo, "/") do
      [owner, _name] ->
        with {:ok, token} <- account_token(c, owner) do
          case request(c, token, :get, "/repos/#{repo}/actions/jobs/#{job_id}", [200, 404]) do
            {:ok, %{"status" => status}} -> {:ok, status}
            {:ok, _not_found} -> {:ok, nil}
            err -> err
          end
        end

      _ ->
        {:error, :invalid_repo}
    end
  end

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
         {:ok, expires_at, _} <- DateTime.from_iso8601(exp) do
      Cache.put_token(c.cache, id, token, expires_at)
      {:ok, token}
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
