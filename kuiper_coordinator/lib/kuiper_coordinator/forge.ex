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

  def registration_token({mod, client}, scope), do: mod.registration_token(client, scope)
  def remove_runner({mod, client}, scope, name), do: mod.remove_runner(client, scope, name)
  def scope_url({mod, client}, scope), do: mod.scope_url(client, scope)
end
