defmodule KuiperCoordinatorWeb.Router do
  use KuiperCoordinatorWeb, :router

  import KuiperCoordinatorWeb.AdminAuth,
    only: [require_admin_enabled: 2, fetch_admin: 2, require_admin: 2]

  pipeline :browser do
    plug :accepts, ["html"]
    plug :fetch_session
    plug :fetch_live_flash
    plug :put_root_layout, html: {KuiperCoordinatorWeb.Layouts, :root}
    plug :protect_from_forgery
    plug :put_secure_browser_headers
  end

  pipeline :admin do
    plug :require_admin_enabled
    plug :fetch_admin
  end

  pipeline :api do
    plug :accepts, ["json"]
  end

  scope "/", KuiperCoordinatorWeb do
    pipe_through :browser

    get "/", PageController, :home
  end

  # Admin UI (parity section 6): only when admin is enabled.
  scope "/admin", KuiperCoordinatorWeb do
    pipe_through [:browser, :admin]

    get "/login", AdminSessionController, :new
    post "/login", AdminSessionController, :create
    delete "/logout", AdminSessionController, :delete
  end

  scope "/admin", KuiperCoordinatorWeb do
    pipe_through [:browser, :admin, :require_admin]

    get "/", AdminSessionController, :index
    get "/forges/github/callback", GitHubAppController, :callback

    live_session :admin, on_mount: {KuiperCoordinatorWeb.AdminAuth, :require_admin} do
      live "/dashboard", Admin.DashboardLive
      live "/users", Admin.UsersLive
      live "/api-tokens", Admin.ApiTokensLive
      live "/agents/:agent_id", Admin.AgentLive
      live "/agents/:agent_id/logs", Admin.AgentLogsLive
      live "/errors", Admin.ErrorsLive
      live "/metrics", Admin.MetricsLive
      live "/forges", Admin.ForgesLive
      live "/forges/provisioning", Admin.ProvisioningLive
      live "/forges/github/new", Admin.GitHubAppNewLive
      live "/forges/forgejo/new", Admin.ForgeFormLive, :new_forgejo
      live "/forges/:id/edit", Admin.ForgeFormLive, :edit
    end
  end

  pipeline :api_token do
    plug :require_admin_enabled
    plug KuiperCoordinatorWeb.Plugs.ApiAuth
  end

  # Read-only JSON API (parity API1-API4): bearer kfapi_ tokens only, served with the admin UI.
  scope "/api/v1", KuiperCoordinatorWeb do
    pipe_through [:api, :api_token]

    get "/status", ApiController, :status
    get "/agents", ApiController, :agents
    get "/agents/:agent_id/logs", ApiController, :agent_logs
  end

  # Enable LiveDashboard and Swoosh mailbox preview in development
  if Application.compile_env(:kuiper_coordinator, :dev_routes) do
    # If you want to use the LiveDashboard in production, you should put
    # it behind authentication and allow only admins to access it.
    # If your application does not have an admins-only section yet,
    # you can use Plug.BasicAuth to set up some basic authentication
    # as long as you are also using SSL (which you should anyway).
    import Phoenix.LiveDashboard.Router

    scope "/dev" do
      pipe_through :browser

      live_dashboard "/dashboard", metrics: KuiperCoordinatorWeb.Telemetry
      forward "/mailbox", Plug.Swoosh.MailboxPreview
    end
  end
end
