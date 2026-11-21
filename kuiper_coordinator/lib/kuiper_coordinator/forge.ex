defmodule KuiperCoordinator.Forge do
  @moduledoc """
  What provisioning needs from a forge (Rust forge.rs trait, parity T1). A forge is passed as
  `{module, client}`; `KuiperCoordinator.GitHub.Client` implements it, Forgejo comes next.
  """
  alias KuiperCoordinator.Forge.RunnerScope

  @type t :: {module(), term()}

  @callback registration_token(client :: term(), RunnerScope.t()) ::
              {:ok, String.t()} | {:error, term()}
  @callback remove_runner(client :: term(), RunnerScope.t(), runner_name :: String.t()) ::
              :ok | {:error, term()}
  @callback scope_url(client :: term(), RunnerScope.t()) :: String.t()

  @typedoc "How to start a single-use job runner: a GitHub JIT config (Forgejo comes next)."
  @type launch :: {:github_jit, String.t()}

  @doc """
  Registers a single-use runner for one job (Rust create_job_runner). `runner_group` is the
  job's configured group (nil = default); an unknown name is an error, never group 1 (D9).
  """
  @callback create_job_runner(
              client :: term(),
              RunnerScope.t(),
              runner_name :: String.t(),
              labels :: [String.t()],
              runner_group :: nil | String.t(),
              job_id :: String.t()
            ) :: {:ok, launch()} | {:error, term()}

  @doc "A job's status in GitHub's terms (`queued`, `waiting`, ...); nil when it's gone."
  @callback job_status(client :: term(), repo :: String.t(), job_id :: String.t()) ::
              {:ok, String.t() | nil} | {:error, term()}

  def registration_token({mod, client}, scope), do: mod.registration_token(client, scope)
  def remove_runner({mod, client}, scope, name), do: mod.remove_runner(client, scope, name)
  def scope_url({mod, client}, scope), do: mod.scope_url(client, scope)

  def create_job_runner({mod, client}, scope, name, labels, group, job_id),
    do: mod.create_job_runner(client, scope, name, labels, group, job_id)

  def job_status({mod, client}, repo, job_id), do: mod.job_status(client, repo, job_id)
end
