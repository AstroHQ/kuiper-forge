defmodule KuiperCoordinatorWeb.AdminSetupController do
  @moduledoc """
  First-run setup (D50): while no admin user exists, /admin/setup makes the first one and logs
  them in. Once any user exists it's a 404 for GET and POST alike; the CLI
  (`KuiperCoordinator.Release.create_admin/1`) stays for resets and recovery.
  """
  use KuiperCoordinatorWeb, :controller

  alias KuiperCoordinator.Admin
  alias KuiperCoordinatorWeb.AdminAuth
  require Logger

  def new(conn, _params) do
    if Admin.setup_needed?(), do: render_form(conn, nil, ""), else: not_found(conn)
  end

  def create(conn, params) do
    username = params["username"] || ""

    case Admin.create_first_user(username, params["password"], params["confirm"]) do
      {:ok, user} ->
        ip = AdminAuth.client_ip(conn)
        meta = %{ip_address: ip, user_agent: conn |> get_req_header("user-agent") |> List.first()}
        {:ok, token, _} = Admin.login(user.username, params["password"], meta)
        Logger.info("admin setup created first user username=#{user.username} ip=#{ip}")

        conn
        |> AdminAuth.log_in(token)
        |> put_status(303)
        |> redirect(to: ~p"/admin/dashboard")

      {:error, :setup_done} ->
        not_found(conn)

      {:error, reason} ->
        render_form(conn, Admin.error_message(reason), username)
    end
  end

  defp not_found(conn) do
    conn |> put_status(404) |> put_view(KuiperCoordinatorWeb.ErrorHTML) |> render(:"404")
  end

  defp render_form(conn, error, username) do
    render(conn, :new, error: error, username: username, page_title: "Set up")
  end
end
