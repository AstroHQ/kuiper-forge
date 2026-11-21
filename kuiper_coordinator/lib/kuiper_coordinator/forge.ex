defmodule KuiperCoordinator.Forge do
  @moduledoc """
  What provisioning needs from a forge (Rust forge.rs trait, parity T1). A forge is passed as
  `{module, client}`; `KuiperCoordinator.GitHub.Client` and `KuiperCoordinator.Forgejo.Client` implement it.
  """
  alias KuiperCoordinator.Forge.RunnerScope

  @type t :: {module(), term()}

  @callback registration_token(client :: term(), RunnerScope.t()) ::
              {:ok, String.t()} | {:error, term()}
  @callback remove_runner(client :: term(), RunnerScope.t(), runner_name :: String.t()) ::
              :ok | {:error, term()}
  @callback scope_url(client :: term(), RunnerScope.t()) :: String.t()

  @doc "Which runner the forge's jobs need: \"github\" or \"forgejo\" (forge.rs runner_kind, R5)."
  @callback runner_kind(client :: term()) :: String.t()

  @typedoc """
  How to start a single-use job runner: a GitHub JIT config, or a Forgejo one-job runner
  (agent.proto ForgejoRunner: instance_url, uuid, token, handle).
  """
  @type launch ::
          {:github_jit, String.t()}
          | {:forgejo,
             %{instance_url: String.t(), uuid: String.t(), token: String.t(), handle: String.t()}}

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

  @doc """
  A job's status looked up in a runner scope's jobs rather than a repository's (for runners
  without a repository, #48). Forgejo only; GitHub answers an error.
  """
  @callback scope_job_status(client :: term(), RunnerScope.t(), job_id :: String.t()) ::
              {:ok, String.t() | nil} | {:error, term()}

  @doc """
  Every job waiting for a runner that the forge can see (recovery scan, parity G9). Failures
  in one installation, repo or run are logged and skipped, never fail the whole scan.
  """
  @callback list_queued_jobs(client :: term()) ::
              {:ok, [KuiperCoordinator.Forge.QueuedJob.t()]} | {:error, term()}

  def registration_token({mod, client}, scope), do: mod.registration_token(client, scope)
  def remove_runner({mod, client}, scope, name), do: mod.remove_runner(client, scope, name)
  def scope_url({mod, client}, scope), do: mod.scope_url(client, scope)
  def runner_kind({mod, client}), do: mod.runner_kind(client)
  def list_queued_jobs({mod, client}), do: mod.list_queued_jobs(client)

  def create_job_runner({mod, client}, scope, name, labels, group, job_id),
    do: mod.create_job_runner(client, scope, name, labels, group, job_id)

  def job_status({mod, client}, repo, job_id), do: mod.job_status(client, repo, job_id)

  def scope_job_status({mod, client}, scope, job_id),
    do: mod.scope_job_status(client, scope, job_id)
end
