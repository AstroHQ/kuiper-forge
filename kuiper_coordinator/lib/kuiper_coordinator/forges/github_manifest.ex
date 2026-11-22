defmodule KuiperCoordinator.Forges.GitHubManifest do
  @moduledoc """
  GitHub App manifest flow (D40): one click in the admin UI creates the GitHub App and
  hands back its ID, private key and webhook secret.

  1. `new_app_url/2` + `manifest/2`: the admin's browser POSTs a form with a `manifest`
     field to github.com/settings/apps/new (or the org's), with a signed `state`.
  2. GitHub creates the App and redirects to `redirect_url?code=...&state=...`.
  3. `verify_state/2` checks the state, `convert/2` exchanges the code (single use, one
     hour) at `POST /app-manifests/{code}/conversions`, which needs no auth.

  Docs: https://docs.github.com/en/apps/sharing-github-apps/registering-a-github-app-from-a-manifest
  """

  @github "https://github.com"
  @api "https://api.github.com"
  @state_salt "kuiper github app manifest"
  # GitHub's codes last an hour; the round trip should take minutes.
  @state_max_age 60 * 60

  # What runner provisioning needs (G1-G8): repo runners (administration), org runners,
  # queued-job scans (actions) and repo listing (metadata), plus workflow_job deliveries.
  @permissions %{
    "administration" => "write",
    "organization_self_hosted_runners" => "write",
    "actions" => "read",
    "metadata" => "read"
  }
  @events ["workflow_job"]

  @type app :: %{
          # the provisioning mode the new forge starts in, set by the callback
          optional(:mode) => :webhook | :polling,
          app_id: integer(),
          slug: String.t(),
          name: String.t(),
          owner: String.t() | nil,
          html_url: String.t(),
          private_key: String.t(),
          webhook_secret: String.t() | nil,
          client_id: String.t() | nil,
          client_secret: String.t() | nil
        }

  @doc "Test and deploy overrides: `:github_url`, `:base_url`, `:req_options`, `:sink`."
  def config, do: Application.get_env(:kuiper_coordinator, :github_manifest, [])

  def permissions, do: @permissions
  def events, do: @events

  @doc """
  The manifest for a new App. `base_url` is the coordinator's public URL (webhooks and the
  redirect come back to it). Options: `:name`, `:public` (default false), `:webhook`
  (default true; off for pool-only setups), `:webhook_path` (default "/webhook"; pass the
  stored `Settings.webhook_path` so the App delivers where the webhook plug listens).
  """
  @spec manifest(String.t(), keyword()) :: map()
  def manifest(base_url, opts \\ []) do
    base = String.trim_trailing(base_url, "/")
    webhook? = Keyword.get(opts, :webhook, true)
    path = Keyword.get(opts, :webhook_path) || "/webhook"

    %{
      "name" => Keyword.get(opts, :name, "Kuiper Forge"),
      "url" => base,
      "description" => "Self-hosted runners provisioned by Kuiper Forge.",
      "redirect_url" => base <> "/admin/forges/github/callback",
      "setup_url" => base <> "/admin/forges",
      "setup_on_update" => true,
      "public" => Keyword.get(opts, :public, false),
      "hook_attributes" => %{"url" => base <> path, "active" => webhook?},
      "default_permissions" => @permissions,
      "default_events" => if(webhook?, do: @events, else: [])
    }
  end

  @doc """
  Where the form posts: the user's account, or an org's (`owner: "org"`), with the state.
  """
  @spec new_app_url(String.t(), keyword()) :: String.t()
  def new_app_url(state, opts \\ []) do
    github = Keyword.get(opts, :github_url, @github)

    path =
      case Keyword.get(opts, :org) do
        org when org in [nil, ""] ->
          "/settings/apps/new"

        org ->
          "/organizations/" <> URI.encode(org, &URI.char_unreserved?/1) <> "/settings/apps/new"
      end

    github <> path <> "?" <> URI.encode_query(%{"state" => state})
  end

  @doc "A signed state for one admin session; `data` comes back from `verify_state/2`."
  @spec sign_state(module() | Plug.Conn.t(), term()) :: String.t()
  def sign_state(endpoint, data), do: Phoenix.Token.sign(endpoint, @state_salt, data)

  @spec verify_state(module() | Plug.Conn.t(), String.t() | nil, keyword()) ::
          {:ok, term()} | {:error, :invalid_state | :expired_state}
  def verify_state(endpoint, state, opts \\ [])
  def verify_state(_endpoint, state, _opts) when state in [nil, ""], do: {:error, :invalid_state}

  def verify_state(endpoint, state, opts) do
    case Phoenix.Token.verify(endpoint, @state_salt, state,
           max_age: Keyword.get(opts, :max_age, @state_max_age)
         ) do
      {:ok, data} -> {:ok, data}
      {:error, :expired} -> {:error, :expired_state}
      {:error, _} -> {:error, :invalid_state}
    end
  end

  @doc """
  Exchange the redirect's code for the App's credentials. Options: `:base_url`,
  `:req_options` (a test plug).
  """
  @spec convert(String.t(), keyword()) :: {:ok, app()} | {:error, term()}
  def convert(code, opts \\ [])

  def convert(code, opts) when is_binary(code) and code != "" do
    if code =~ ~r/\A[A-Za-z0-9_-]+\z/ do
      req =
        Req.new(
          base_url: Keyword.get(opts, :base_url, @api),
          headers: [
            {"accept", "application/vnd.github+json"},
            {"x-github-api-version", "2022-11-28"},
            {"user-agent", "kuiper-forge"}
          ],
          retry: false
        )
        |> Req.merge(Keyword.get(opts, :req_options, []))

      case Req.post(req, url: "/app-manifests/#{code}/conversions") do
        {:ok, %{status: 201, body: %{} = body}} -> parse(body)
        {:ok, %{status: status, body: body}} -> {:error, {:http, status, message(body)}}
        {:error, e} -> {:error, e}
      end
    else
      {:error, :invalid_code}
    end
  end

  def convert(_code, _opts), do: {:error, :invalid_code}

  defp parse(%{"id" => id, "pem" => pem} = b) when is_integer(id) and is_binary(pem) do
    {:ok,
     %{
       app_id: id,
       slug: b["slug"],
       name: b["name"],
       owner: get_in(b, ["owner", "login"]),
       html_url: b["html_url"],
       private_key: pem,
       webhook_secret: b["webhook_secret"],
       client_id: b["client_id"],
       client_secret: b["client_secret"]
     }}
  end

  defp parse(_), do: {:error, :bad_response}

  defp message(%{"message" => m}), do: m
  defp message(_), do: nil

  @doc "Where the admin installs the new App and picks its repos."
  @spec install_url(app() | map()) :: String.t()
  def install_url(%{html_url: url}) when is_binary(url), do: url <> "/installations/new"
end
