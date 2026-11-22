defmodule KuiperCoordinatorWeb.Admin.GitHubAppNewLive do
  @moduledoc """
  Add a GitHub forge (D40): pick the App's name, the org (blank = your account) and the
  coordinator's public URL, then the browser POSTs the manifest to GitHub with a signed state.
  GitHub comes back to `GitHubAppController.callback/2`.
  """
  use KuiperCoordinatorWeb, :live_view

  alias KuiperCoordinator.Forges.GitHubManifest

  @impl true
  def mount(_params, _session, socket) do
    params = %{
      "name" => "Kuiper Forge",
      "org" => "",
      "base_url" => KuiperCoordinatorWeb.Endpoint.url(),
      "webhook" => "true"
    }

    {:ok,
     assign(socket,
       page_title: "Add GitHub forge",
       form: to_form(params, as: :app),
       post: nil
     )}
  end

  @impl true
  def handle_event("create", %{"app" => params}, socket) do
    case validate(params) do
      {:ok, %{name: name, org: org, base_url: base, webhook: webhook?}} ->
        state =
          GitHubManifest.sign_state(KuiperCoordinatorWeb.Endpoint, %{
            "admin" => socket.assigns.current_admin.id,
            "webhook" => webhook?
          })

        manifest =
          GitHubManifest.manifest(base,
            name: name,
            webhook: webhook?,
            webhook_path: KuiperCoordinator.Forges.get_settings().webhook_path
          )

        opts = Keyword.take(GitHubManifest.config(), [:github_url]) ++ [org: org]

        {:noreply,
         socket
         |> clear_flash()
         |> assign(
           form: to_form(params, as: :app),
           post: %{
             url: GitHubManifest.new_app_url(state, opts),
             manifest: Jason.encode!(manifest)
           }
         )}

      {:error, message} ->
        {:noreply,
         socket
         |> put_flash(:error, message)
         |> assign(form: to_form(params, as: :app), post: nil)}
    end
  end

  defp validate(params) do
    name = String.trim(to_s(params["name"]))
    org = String.trim(to_s(params["org"]))
    base = String.trim(to_s(params["base_url"]))
    uri = URI.parse(base)

    cond do
      name == "" or String.length(name) > 34 ->
        {:error, "App name must be 1-34 characters (GitHub's limit)"}

      org != "" and not (org =~ ~r/\A[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})\z/) ->
        {:error, "Organization must be a GitHub login"}

      uri.scheme not in ["http", "https"] or uri.host in [nil, ""] ->
        {:error, "Public URL must be an http(s) URL GitHub can reach"}

      true ->
        {:ok, %{name: name, org: org, base_url: base, webhook: params["webhook"] == "true"}}
    end
  end

  defp to_s(v) when is_binary(v), do: v
  defp to_s(_), do: ""

  @impl true
  def render(assigns) do
    ~H"""
    <Layouts.admin flash={@flash} current_admin={@current_admin} active={:forges}>
      <.header>
        Add GitHub forge
        <:subtitle>
          Creates a GitHub App from a manifest. GitHub hands back its ID, private key and
          webhook secret, and you then install it on the repos or org it should serve.
        </:subtitle>
      </.header>

      <.form for={@form} id="github-app-form" phx-submit="create" class="max-w-xl space-y-2">
        <.input field={@form[:name]} label="App name" />
        <.input field={@form[:org]} label="Organization (blank for your own account)" />
        <.input
          field={@form[:base_url]}
          label="Coordinator public URL (webhooks and the redirect come here)"
        />
        <.input
          field={@form[:webhook]}
          type="checkbox"
          label="Webhook mode (workflow_job events); off to poll for jobs instead"
        />
        <.button class="btn btn-primary">Create GitHub App</.button>
      </.form>

      <form
        :if={@post}
        id="github-manifest-post"
        action={@post.url}
        method="post"
        phx-trigger-action
      >
        <input type="hidden" name="manifest" value={@post.manifest} />
        <p class="text-sm">Sending you to GitHub…</p>
      </form>
    </Layouts.admin>
    """
  end
end
