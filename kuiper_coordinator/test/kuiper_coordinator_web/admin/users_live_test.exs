defmodule KuiperCoordinatorWeb.Admin.UsersLiveTest do
  @moduledoc """
  The admin users page (parity UI8): list, create (name rules, confirm), set password (ends that
  user's sessions but keeps your own), delete (not yourself; never the last admin).
  """
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log
  import Phoenix.LiveViewTest

  alias KuiperCoordinator.Admin

  setup %{conn: conn} do
    old = Application.get_env(:kuiper_coordinator, :admin)
    Application.put_env(:kuiper_coordinator, :admin, session_timeout_secs: 3600)
    on_exit(fn -> Application.put_env(:kuiper_coordinator, :admin, old || []) end)
    Admin.LoginThrottle.reset()
    {:ok, _} = Admin.create_user("alice", "correct horse", "correct horse")
    conn = post(conn, ~p"/admin/login", %{"username" => "alice", "password" => "correct horse"})
    %{conn: recycle(conn)}
  end

  defp create(view, name, pw, confirm) do
    view
    |> form("#create-user", user: %{username: name, password: pw, confirm: confirm})
    |> render_submit()
  end

  test "needs a login", %{conn: _} do
    conn = build_conn()
    assert {:error, {:redirect, %{to: "/admin/login"}}} = live(conn, ~p"/admin/users")
  end

  test "lists users and creates one", %{conn: conn} do
    {:ok, view, html} = live(conn, ~p"/admin/users")
    assert html =~ "alice"

    html = create(view, "  bob.ci@example  ", "pw1", "pw1")
    assert html =~ "Created bob.ci@example"
    assert has_element?(view, "#user-bob\\.ci\\@example")
    assert Admin.get_user("bob.ci@example")
  end

  test "create errors: bad name, mismatch, empty, taken", %{conn: conn} do
    {:ok, view, _} = live(conn, ~p"/admin/users")
    assert create(view, "bad name<script>", "x", "x") =~ "1-64 characters"
    assert create(view, String.duplicate("a", 65), "x", "x") =~ "1-64 characters"
    assert create(view, "carol", "x", "y") =~ "Passwords don&#39;t match"
    assert create(view, "carol", "", "") =~ "Password can&#39;t be empty"
    assert create(view, "alice", "x", "x") =~ "already exists"
    refute Admin.get_user("carol")
  end

  test "setting another user's password ends their sessions", %{conn: conn} do
    {:ok, _} = Admin.create_user("bob", "old", "old")
    {:ok, bob_session, _} = Admin.login("bob", "old", %{})
    {:ok, view, _} = live(conn, ~p"/admin/users")

    html =
      view
      |> form("#password-bob", pw: %{password: "new", confirm: "new"})
      |> render_submit()

    assert html =~ "Password set for bob"
    assert Admin.session_user(bob_session) == nil
    assert {:ok, _, _} = Admin.login("bob", "new", %{})
  end

  test "setting your own password keeps your session, ends your others", %{conn: conn} do
    {:ok, other, _} = Admin.login("alice", "correct horse", %{})
    {:ok, view, _} = live(conn, ~p"/admin/users")

    html =
      view
      |> form("#password-alice", pw: %{password: "new pw", confirm: "new pw"})
      |> render_submit()

    assert html =~ "Password set for alice"
    assert Admin.session_user(other) == nil
    # still signed in on this connection
    assert {:ok, _, _} = live(conn, ~p"/admin/users")
  end

  test "password mismatch leaves it unchanged", %{conn: conn} do
    {:ok, view, _} = live(conn, ~p"/admin/users")

    assert view
           |> form("#password-alice", pw: %{password: "a", confirm: "b"})
           |> render_submit() =~ "Passwords don&#39;t match"

    assert {:ok, _, _} = Admin.login("alice", "correct horse", %{})
  end

  test "delete: others yes, yourself no", %{conn: conn} do
    {:ok, _} = Admin.create_user("bob", "pw", "pw")
    {:ok, view, _} = live(conn, ~p"/admin/users")

    refute has_element?(view, "#delete-alice")
    assert view |> element("#delete-bob") |> render_click() =~ "Deleted bob"
    refute Admin.get_user("bob")

    # a forged event for yourself is refused
    assert render_click(view, "delete", %{"username" => "alice"}) =~ "can&#39;t delete yourself"
    assert Admin.get_user("alice")
  end

  test "deleting a user who's already gone says so", %{conn: conn} do
    {:ok, view, _} = live(conn, ~p"/admin/users")
    assert render_click(view, "delete", %{"username" => "ghost"}) =~ "No such user"
  end
end
