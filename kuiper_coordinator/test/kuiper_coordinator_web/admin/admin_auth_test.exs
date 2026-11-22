defmodule KuiperCoordinatorWeb.AdminAuthTest do
  @moduledoc """
  The admin login flow (parity UI1, UI10): /admin is always served (D49: no on/off switch),
  /admin goes to the dashboard, unauthenticated requests get a 303 to the login page, an expired
  session goes back to login, logout ends the session, and repeated failed logins are throttled.
  """
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log
  import Phoenix.LiveViewTest

  alias KuiperCoordinator.Admin

  setup do
    old = Application.get_env(:kuiper_coordinator, :admin)
    Application.put_env(:kuiper_coordinator, :admin, session_timeout_secs: 3600)
    on_exit(fn -> Application.put_env(:kuiper_coordinator, :admin, old || []) end)
    KuiperCoordinator.Admin.LoginThrottle.reset()
    {:ok, _} = Admin.create_user("alice", "correct horse", "correct horse")
    :ok
  end

  defp login(conn, user \\ "alice", pw \\ "correct horse") do
    post(conn, ~p"/admin/login", %{"username" => user, "password" => pw})
  end

  test "admin routes are served with no admin config at all, login still required (D49)",
       %{conn: conn} do
    Application.delete_env(:kuiper_coordinator, :admin)
    assert conn |> get(~p"/admin/login") |> html_response(200) =~ "password"
    conn = get(conn, ~p"/admin/dashboard")
    assert redirected_to(conn, 303) == ~p"/admin/login"
    assert {:error, {:redirect, %{to: "/admin/login"}}} = live(build_conn(), ~p"/admin/metrics")
  end

  test "unauthenticated requests get a 303 to the login page", %{conn: conn} do
    for path <- [~p"/admin", ~p"/admin/dashboard"] do
      conn = get(conn, path)
      assert conn.status == 303
      assert redirected_to(conn, 303) == ~p"/admin/login"
    end
  end

  test "login with the right password lands on the dashboard", %{conn: conn} do
    conn = login(conn)
    assert redirected_to(conn, 303) == ~p"/admin/dashboard"

    conn = get(recycle(conn), ~p"/admin")
    assert redirected_to(conn) == ~p"/admin/dashboard"

    {:ok, _view, html} = live(recycle(conn), ~p"/admin/dashboard")
    assert html =~ "alice"
  end

  test "a wrong password or unknown user shows one generic error", %{conn: conn} do
    for {u, p} <- [{"alice", "nope"}, {"mallory", "correct horse"}] do
      html = conn |> login(u, p) |> html_response(200)
      assert html =~ "Invalid username or password"
    end
  end

  test "an expired session redirects to login", %{conn: conn} do
    conn = login(conn)

    KuiperCoordinator.Repo.update_all(Admin.Session,
      set: [expires_at: DateTime.add(DateTime.utc_now(), -1, :second)]
    )

    conn = get(recycle(conn), ~p"/admin/dashboard")
    assert redirected_to(conn, 303) == ~p"/admin/login"
  end

  test "logout deletes the session and the old cookie stops working", %{conn: conn} do
    conn = login(conn)
    old = recycle(conn)
    conn = delete(recycle(conn), ~p"/admin/logout")
    assert redirected_to(conn, 303) == ~p"/admin/login"
    assert KuiperCoordinator.Repo.aggregate(Admin.Session, :count) == 0
    # replaying the pre-logout cookie no longer gets in
    assert old |> get(~p"/admin/dashboard") |> redirected_to(303) == ~p"/admin/login"
  end

  test "the login form is CSRF protected", %{conn: conn} do
    conn = conn |> get(~p"/admin/login")
    assert html_response(conn, 200) =~ ~s(name="_csrf_token")
  end

  test "failed logins from one address are throttled (UI10)", %{conn: conn} do
    for _ <- 1..10, do: assert(login(conn, "alice", "nope").status == 200)
    conn = login(conn)
    assert html_response(conn, 429) =~ "Too many failed logins"
  end
end
