defmodule KuiperCoordinatorWeb.AdminSessionController do
  @moduledoc "Admin login and logout (Rust admin/routes.rs login_page, login_submit, logout)."
  use KuiperCoordinatorWeb, :controller

  alias KuiperCoordinator.Admin
  alias KuiperCoordinator.Admin.LoginThrottle
  alias KuiperCoordinatorWeb.AdminAuth
  require Logger

  def index(conn, _params), do: redirect(conn, to: ~p"/admin/dashboard")

  def new(%{assigns: %{current_admin: %Admin.User{}}} = conn, _params),
    do: redirect(conn, to: ~p"/admin/dashboard")

  def new(conn, _params), do: render_form(conn, 200, nil, "")

  def create(conn, %{"username" => username, "password" => password}) do
    ip = AdminAuth.client_ip(conn)

    if LoginThrottle.allowed?(ip) do
      meta = %{ip_address: ip, user_agent: conn |> get_req_header("user-agent") |> List.first()}

      case Admin.login(username, password, meta) do
        {:ok, token, user} ->
          LoginThrottle.succeeded(ip)
          Logger.info("admin login username=#{user.username} ip=#{ip}")

          conn
          |> AdminAuth.log_in(token)
          |> put_status(303)
          |> redirect(to: ~p"/admin/dashboard")

        :error ->
          LoginThrottle.failed(ip)
          Logger.warning("admin login failed username=#{inspect(username)} ip=#{ip}")
          render_form(conn, 200, "Invalid username or password", username)
      end
    else
      render_form(conn, 429, "Too many failed logins. Try again in a few minutes.", username)
    end
  end

  def create(conn, _params), do: render_form(conn, 200, "Invalid username or password", "")

  def delete(conn, _params) do
    conn
    |> AdminAuth.log_out()
    |> put_status(303)
    |> redirect(to: ~p"/admin/login")
  end

  defp render_form(conn, status, error, username) do
    conn
    |> put_status(status)
    |> render(:new, error: error, username: username, page_title: "Sign in")
  end
end
