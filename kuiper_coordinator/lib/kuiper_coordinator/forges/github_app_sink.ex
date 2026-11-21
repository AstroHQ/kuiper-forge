defmodule KuiperCoordinator.Forges.GitHubAppSink do
  @moduledoc """
  Where the manifest flow's new GitHub App goes (D40/D41). The Forges context in #88 is the
  real sink; until it lands the default refuses, so no App credentials are dropped silently.
  Set with `config :kuiper_coordinator, :github_manifest, sink: Module`.
  """

  alias KuiperCoordinator.Forges.GitHubManifest

  @callback save_github_app(GitHubManifest.app(), admin_username :: String.t()) ::
              {:ok, term()} | {:error, term()}

  @behaviour __MODULE__

  @impl true
  def save_github_app(_app, _admin), do: {:error, :forge_storage_not_ready}

  @doc "The configured sink."
  def impl, do: Keyword.get(GitHubManifest.config(), :sink, __MODULE__)
end
