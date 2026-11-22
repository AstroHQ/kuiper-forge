defmodule KuiperCoordinatorWeb.GitHubAppController do
  @moduledoc """
  GitHub's redirect back from the App manifest flow (D40). The state must be one this
  coordinator signed for the admin who's signed in; only then is the single-use code
  exchanged and the App handed to the forge sink. Then on to GitHub to install it.
  """
  use KuiperCoordinatorWeb, :controller

  require Logger

  alias KuiperCoordinator.Forges.{GitHubAppSink, GitHubManifest}

  def callback(conn, params) do
    admin = conn.assigns.current_admin
    opts = Keyword.take(GitHubManifest.config(), [:base_url, :req_options])

    with {:ok, %{"admin" => id}} when id == admin.id <-
           GitHubManifest.verify_state(conn, str(params["state"])),
         {:convert, {:ok, app}} <- {:convert, GitHubManifest.convert(str(params["code"]), opts)},
         {:save, {:ok, _}} <- {:save, GitHubAppSink.impl().save_github_app(app, admin.username)} do
      Logger.info("GitHub App #{app.slug} (#{app.app_id}) created by #{admin.username}")
      redirect(conn, external: GitHubManifest.install_url(app))
    else
      {:convert, {:error, reason}} ->
        Logger.warning("GitHub App manifest conversion failed: #{inspect(reason)}")
        back(conn, "GitHub didn't hand over the App (#{describe(reason)}). Start again.")

      {:save, {:error, reason}} ->
        Logger.error("GitHub App created but not saved: #{inspect(reason)}")

        back(
          conn,
          "The App was created on GitHub but couldn't save it (#{inspect(reason)}). " <>
            "Delete it on GitHub and start again."
        )

      _ ->
        back(conn, "That GitHub link isn't valid for this session. Please start again.")
    end
  end

  defp back(conn, message) do
    conn |> put_flash(:error, message) |> redirect(to: ~p"/admin/forges/github/new")
  end

  defp describe({:http, status, msg}), do: "HTTP #{status}#{if msg, do: ": " <> msg}"
  defp describe(:invalid_code), do: "bad code"
  defp describe(:bad_response), do: "unexpected reply"
  defp describe(other), do: inspect(other)

  defp str(v) when is_binary(v), do: v
  defp str(_), do: nil
end
