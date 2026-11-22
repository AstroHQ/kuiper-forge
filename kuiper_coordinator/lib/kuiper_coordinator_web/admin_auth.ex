defmodule KuiperCoordinatorWeb.AdminAuth do
  @moduledoc """
  Admin UI auth (parity UI1): plugs for the `/admin` scope and the LiveView `on_mount` hook.

  * `:fetch_admin` puts `:current_admin` (or nil) from the session's admin session id.
  * `:redirect_to_setup` sends every request to `/admin/setup` while there are no users (D50).
  * `:require_admin` sends a 303 to the login page when there's no live session, and the login
    comes back to the page that was asked for.

  The session id is in the (signed, encrypted, SameSite=Lax) Phoenix session cookie; the row
  in `admin_sessions` is the source of truth, so logout or expiry ends it on every request and
  every LiveView mount.
  """
  use KuiperCoordinatorWeb, :verified_routes
  import Plug.Conn
  import Phoenix.Controller

  alias KuiperCoordinator.Admin

  @session_key "admin_session"

  # the page a logged-out GET was for, so the login can carry on to it. GitHub's App callback (D40)
  # has a single-use code in its query, and going to the dashboard instead throws it away
  @return_key "admin_return_to"

  def session_key, do: @session_key

  def fetch_admin(conn, _opts) do
    assign(conn, :current_admin, Admin.session_user(get_session(conn, @session_key)))
  end

  @doc """
  First-run setup (D50): while no admin user exists, every /admin request but the setup page
  itself gets a 303 to /admin/setup.
  """
  def redirect_to_setup(%{request_path: "/admin/setup"} = conn, _opts), do: conn

  def redirect_to_setup(conn, _opts) do
    if Admin.setup_needed?() do
      conn |> put_status(303) |> redirect(to: ~p"/admin/setup") |> halt()
    else
      conn
    end
  end

  def require_admin(%{assigns: %{current_admin: %Admin.User{}}} = conn, _opts), do: conn

  def require_admin(conn, _opts) do
    conn
    |> remember_path()
    |> put_status(303)
    |> redirect(to: ~p"/admin/login")
    |> halt()
  end

  # only plain /admin paths are kept: `redirect(to:)` raises on ones it doesn't like (`//`, `/%`, a
  # backslash), and a link someone crafted shouldn't end a login in a 500
  @returnable ~r{\A/admin(/[\w.~-]+)*(\?[\w.~=&-]*)?\z}

  defp remember_path(%{method: "GET"} = conn) do
    query = if conn.query_string == "", do: "", else: "?" <> conn.query_string
    path = conn.request_path <> query

    if path =~ @returnable, do: put_session(conn, @return_key, path), else: conn
  end

  defp remember_path(conn), do: conn

  @doc """
  Returns the page that sent this browser to the login page. Otherwise the dashboard. Read it
  before `log_in/2`, which clears the session.
  """
  def return_path(conn), do: get_session(conn, @return_key) || ~p"/admin/dashboard"

  @doc "Starts a session for `token`: a fresh session cookie, so no fixation."
  def log_in(conn, token) do
    conn
    |> configure_session(renew: true)
    |> clear_session()
    |> put_session(@session_key, token)
  end

  def log_out(conn) do
    Admin.logout(get_session(conn, @session_key))

    if live_socket_id = get_session(conn, :live_socket_id) do
      KuiperCoordinatorWeb.Endpoint.broadcast(live_socket_id, "disconnect", %{})
    end

    configure_session(conn, drop: true)
  end

  @doc "The client address for throttling and the session row."
  def client_ip(conn), do: conn.remote_ip |> :inet.ntoa() |> to_string()

  ## LiveView

  def on_mount(:require_admin, _params, session, socket) do
    case Admin.session_user(session[@session_key]) do
      %Admin.User{} = admin ->
        {:cont,
         Phoenix.Component.assign(socket,
           current_admin: admin,
           admin_session_id: session[@session_key]
         )}

      _ ->
        {:halt, Phoenix.LiveView.redirect(socket, to: ~p"/admin/login")}
    end
  end
end
