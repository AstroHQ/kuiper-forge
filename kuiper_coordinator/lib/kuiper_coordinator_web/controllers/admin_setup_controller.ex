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
    case fields(params) do
      {:ok, username, password, confirm} -> create(conn, username, password, confirm)
      :error -> render_form(conn, "Fill in a username and password", "")
    end
  end

  defp create(conn, username, password, confirm) do
    case Admin.create_first_user(username, password, confirm) do
      {:ok, user} ->
        ip = AdminAuth.client_ip(conn)
        meta = %{ip_address: ip, user_agent: conn |> get_req_header("user-agent") |> List.first()}
        {:ok, token, _} = Admin.login(user.username, password, meta)
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

  # The endpoint is unauthenticated: take flat strings only (no `setup[username][x]=y`).
  defp fields(%{"setup" => %{"username" => u, "password" => p, "confirm" => c}})
       when is_binary(u) and is_binary(p) and is_binary(c),
       do: {:ok, u, p, c}

  defp fields(_), do: :error

  defp not_found(conn) do
    conn |> put_status(404) |> put_view(KuiperCoordinatorWeb.ErrorHTML) |> render(:"404")
  end

  defp render_form(conn, error, username) do
    form = Phoenix.Component.to_form(%{"username" => username}, as: :setup)
    render(conn, :new, error: error, form: form, page_title: "Set up")
  end
end
