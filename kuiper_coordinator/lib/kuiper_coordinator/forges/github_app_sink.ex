defmodule KuiperCoordinator.Forges.GitHubAppSink do
  @moduledoc """
  Where the manifest flow's new GitHub App goes (D40/D41): stored as a GitHub forge through
  `KuiperCoordinator.Forges`. The App is saved without the live check, since GitHub only
  installs it after this (the check wants an installation); the Forges page shows the fleet's
  state once it's installed. The forge is named after the App's slug, with the App ID added
  when that name is taken. Override with `config :kuiper_coordinator, :github_manifest,
  sink: Module` (tests).
  """

  require Logger

  alias KuiperCoordinator.Forges
  alias KuiperCoordinator.Forges.GitHubManifest

  @callback save_github_app(GitHubManifest.app(), admin_username :: String.t()) ::
              {:ok, term()} | {:error, term()}

  @behaviour __MODULE__

  @impl true
  def save_github_app(app, admin) do
    name = if Forges.get_forge_by_name(app.slug), do: "#{app.slug}-#{app.app_id}", else: app.slug

    attrs = %{
      name: name,
      type: :github,
      app_id: app.app_id,
      private_key: app.private_key,
      webhook_secret: app.webhook_secret
    }

    case Forges.create_forge(attrs, validate: false) do
      {:ok, forge} ->
        Logger.info("Stored GitHub App #{app.slug} as forge #{forge.name} (by #{admin})")
        {:ok, forge}

      {:error, %Ecto.Changeset{} = cs} ->
        {:error, errors(cs)}
    end
  end

  defp errors(cs) do
    cs
    |> Ecto.Changeset.traverse_errors(fn {msg, _} -> msg end)
    |> Enum.map_join("; ", fn {k, v} -> "#{k} #{Enum.join(v, ", ")}" end)
  end

  @doc "The configured sink."
  def impl, do: Keyword.get(GitHubManifest.config(), :sink, __MODULE__)
end
