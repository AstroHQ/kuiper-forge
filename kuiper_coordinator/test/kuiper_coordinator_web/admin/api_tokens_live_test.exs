defmodule KuiperCoordinatorWeb.Admin.ApiTokensLiveTest do
  @moduledoc "The admin API tokens page (parity UI9): create (shown once), list by prefix, delete."
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log
  import Phoenix.LiveViewTest

  alias KuiperCoordinator.Admin
  alias KuiperCoordinator.Admin.ApiTokens

  setup %{conn: conn} do
    old = Application.get_env(:kuiper_coordinator, :admin)
    Application.put_env(:kuiper_coordinator, :admin, session_timeout_secs: 3600)
    on_exit(fn -> Application.put_env(:kuiper_coordinator, :admin, old || []) end)
    Admin.LoginThrottle.reset()
    {:ok, _} = Admin.create_user("alice", "correct horse", "correct horse")
    conn = post(conn, ~p"/admin/login", %{"username" => "alice", "password" => "correct horse"})
    %{conn: recycle(conn)}
  end

  test "needs a login" do
    assert {:error, {:redirect, %{to: "/admin/login"}}} =
             live(build_conn(), ~p"/admin/api-tokens")
  end

  test "creates a token, shows it once, and it works on the API", %{conn: conn} do
    {:ok, view, _} = live(conn, ~p"/admin/api-tokens")

    html = view |> form("#create-token", token: %{name: "grafana"}) |> render_submit()
    [token] = Regex.run(~r/kfapi_[A-Za-z0-9]{40}/, html)
    assert html =~ "won&#39;t be shown again"

    [record] = ApiTokens.list()
    assert record.name == "grafana" and record.created_by == "alice"
    assert has_element?(view, "#token-#{record.id}", record.token_prefix)

    # a fresh page load never has the secret
    {:ok, _, html} = live(conn, ~p"/admin/api-tokens")
    refute html =~ token

    api = build_conn() |> put_req_header("authorization", "Bearer #{token}")
    assert json_response(get(api, ~p"/api/v1/status"), 200)["registered_agents"] == 0
  end

  test "rejects a bad name", %{conn: conn} do
    {:ok, view, _} = live(conn, ~p"/admin/api-tokens")
    html = view |> form("#create-token", token: %{name: "  "}) |> render_submit()
    assert html =~ "1-100 characters"
    assert ApiTokens.list() == []
  end

  test "deletes a token", %{conn: conn} do
    {:ok, token, record} = ApiTokens.create("old", "alice")
    {:ok, view, _} = live(conn, ~p"/admin/api-tokens")
    html = view |> element("#delete-#{record.id}") |> render_click()
    assert html =~ "Deleted old"
    refute has_element?(view, "#token-#{record.id}")
    assert ApiTokens.validate(token) == nil
  end

  test "the nav links to it", %{conn: conn} do
    {:ok, _, html} = live(conn, ~p"/admin/dashboard")
    assert html =~ ~s(href="/admin/api-tokens")
  end
end
