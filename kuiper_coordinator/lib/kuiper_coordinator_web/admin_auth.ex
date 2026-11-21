defmodule KuiperCoordinatorWeb.AdminAuth do
  @moduledoc """
  Admin UI auth (parity UI1): plugs for the `/admin` scope and the LiveView `on_mount` hook.

  * `:require_admin_enabled` 404s everything under `/admin` unless admin is enabled.
  * `:fetch_admin` puts `:current_admin` (or nil) from the session's admin session id.
  * `:require_admin` sends a 303 to the login page when there's no live session.

  The session id is in the (signed, encrypted, SameSite=Strict) Phoenix session cookie; the row
  in `admin_sessions` is the source of truth, so logout or expiry ends it on every request and
  every LiveView mount.
  """
  use KuiperCoordinatorWeb, :verified_routes
  import Plug.Conn
  import Phoenix.Controller

  alias KuiperCoordinator.Admin

  @session_key "admin_session"

  def session_key, do: @session_key

  def require_admin_enabled(conn, _opts) do
    if Admin.enabled?() do
      conn
    else
      conn |> send_resp(404, "Not Found") |> halt()
    end
  end

  def fetch_admin(conn, _opts) do
    assign(conn, :current_admin, Admin.session_user(get_session(conn, @session_key)))
  end

  def require_admin(%{assigns: %{current_admin: %Admin.User{}}} = conn, _opts), do: conn

  def require_admin(conn, _opts) do
    conn
    |> put_status(303)
    |> redirect(to: ~p"/admin/login")
    |> halt()
  end

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
    with true <- Admin.enabled?(),
         %Admin.User{} = admin <- Admin.session_user(session[@session_key]) do
      {:cont,
       Phoenix.Component.assign(socket,
         current_admin: admin,
         admin_session_id: session[@session_key]
       )}
    else
      _ -> {:halt, Phoenix.LiveView.redirect(socket, to: ~p"/admin/login")}
    end
  end
end
