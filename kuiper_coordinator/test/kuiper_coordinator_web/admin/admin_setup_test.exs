defmodule KuiperCoordinatorWeb.AdminSetupTest do
  @moduledoc """
  First-run setup (D50): with no admin users, every /admin request goes to /admin/setup, which
  creates the first user and logs them in. Once any user exists, setup is gone for GET and POST,
  and two racing setups can't both make an admin.
  """
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log

  alias KuiperCoordinator.Admin

  setup do
    Admin.LoginThrottle.reset()
    :ok
  end

  @fields %{"username" => "root", "password" => "first pass", "confirm" => "first pass"}
  @form %{"setup" => @fields}

  defp form(changes), do: %{"setup" => Map.merge(@fields, changes)}

  test "with no users, /admin and /admin/login go to the setup form", %{conn: conn} do
    for path <- [~p"/admin", ~p"/admin/login", ~p"/admin/dashboard"] do
      assert conn |> get(path) |> redirected_to(303) == ~p"/admin/setup"
    end

    html = conn |> get(~p"/admin/setup") |> html_response(200)
    assert html =~ "Create the first admin"
    assert html =~ ~s(name="setup[username]")
    assert html =~ ~s(name="setup[confirm]")
    assert html =~ "_csrf_token"
  end

  test "setup creates the first user, logs them in, then login works", %{conn: conn} do
    conn = post(conn, ~p"/admin/setup", @form)
    assert redirected_to(conn, 303) == ~p"/admin/dashboard"
    assert [%{username: "root"}] = Admin.list_users()

    # the new session is live
    assert conn |> recycle() |> get(~p"/admin/dashboard") |> html_response(200)

    # and a fresh login with the same password works
    conn =
      build_conn()
      |> post(~p"/admin/login", %{"username" => "root", "password" => "first pass"})

    assert redirected_to(conn, 303) == ~p"/admin/dashboard"
  end

  test "setup keeps the password rules and makes no user on a bad form", %{conn: conn} do
    html =
      conn
      |> post(~p"/admin/setup", form(%{"confirm" => "other"}))
      |> html_response(200)

    assert html =~ "Passwords don&#39;t match"

    html =
      conn |> post(~p"/admin/setup", form(%{"username" => "bad name"})) |> html_response(200)

    assert html =~ "Usernames are 1-64 characters"
    assert Admin.user_count() == 0
  end

  test "once a user exists, setup is refused for GET and POST", %{conn: conn} do
    {:ok, _} = Admin.create_user("alice", "correct horse", "correct horse")

    assert conn |> get(~p"/admin/setup") |> response(404)
    assert conn |> post(~p"/admin/setup", @form) |> response(404)
    assert [%{username: "alice"}] = Admin.list_users()
    assert conn |> get(~p"/admin/login") |> html_response(200) =~ "Sign in"
  end

  test "a second setup POST doesn't make a second admin", %{conn: conn} do
    assert conn |> post(~p"/admin/setup", @form) |> redirected_to(303) == ~p"/admin/dashboard"
    other = form(%{"username" => "mallory"})
    assert build_conn() |> post(~p"/admin/setup", other) |> response(404)
    assert [%{username: "root"}] = Admin.list_users()
  end

  test "racing first-user creates make exactly one admin" do
    results =
      1..8
      |> Enum.map(fn i ->
        Task.async(fn -> Admin.create_first_user("racer#{i}", "pw pw pw", "pw pw pw") end)
      end)
      |> Enum.map(&Task.await(&1, 30_000))

    assert Enum.count(results, &match?({:ok, _}, &1)) == 1
    assert Enum.count(results, &(&1 == {:error, :setup_done})) == 7
    assert Admin.user_count() == 1
  end

  test "nested or non-string params get the form back, not a 500, and make no user", %{
    conn: conn
  } do
    bad = [
      form(%{"username" => %{"x" => "y"}}),
      form(%{"password" => %{"x" => "y"}, "confirm" => %{"x" => "y"}}),
      form(%{"confirm" => ["a", "b"]}),
      %{"setup" => "flat"},
      %{"setup" => ["a"]},
      %{"username" => %{"x" => "y"}},
      %{}
    ]

    for params <- bad do
      html = conn |> post(~p"/admin/setup", params) |> html_response(200)
      assert html =~ ~s(id="setup-error"), inspect(params)
      assert html =~ ~s(id="setup-form")
    end

    assert Admin.user_count() == 0
    # still open, and a good form still works
    assert conn |> post(~p"/admin/setup", @form) |> redirected_to(303) == ~p"/admin/dashboard"
  end
end
