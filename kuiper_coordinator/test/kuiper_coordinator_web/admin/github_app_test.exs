defmodule KuiperCoordinatorWeb.Admin.GitHubAppTest do
  @moduledoc """
  Add a GitHub forge with the App manifest flow (D40): the new-App page posts the manifest
  to GitHub with a signed state, and the callback exchanges the code (stubbed GitHub) and
  hands the App to the forge sink.
  """
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log
  import Phoenix.LiveViewTest

  alias KuiperCoordinator.Admin
  alias KuiperCoordinator.Forges.GitHubManifest

  defmodule Sink do
    @behaviour KuiperCoordinator.Forges.GitHubAppSink
    @impl true
    def save_github_app(app, admin) do
      send(Process.get(:test_pid) || self(), {:saved, app, admin})
      Application.get_env(:kuiper_coordinator, :test_sink_result, {:ok, %{id: 7}})
    end
  end

  setup %{conn: conn} do
    old_admin = Application.get_env(:kuiper_coordinator, :admin)
    old_gh = Application.get_env(:kuiper_coordinator, :github_manifest)
    Application.put_env(:kuiper_coordinator, :admin, session_timeout_secs: 3600)

    on_exit(fn ->
      Application.put_env(:kuiper_coordinator, :admin, old_admin || [])
      Application.put_env(:kuiper_coordinator, :github_manifest, old_gh || [])
      Application.delete_env(:kuiper_coordinator, :test_sink_result)
    end)

    Admin.LoginThrottle.reset()
    {:ok, alice} = Admin.create_user("alice", "correct horse", "correct horse")
    conn = post(conn, ~p"/admin/login", %{"username" => "alice", "password" => "correct horse"})
    %{conn: recycle(conn), alice: alice}
  end

  defp stub_github(fun) do
    Application.put_env(:kuiper_coordinator, :github_manifest,
      sink: Sink,
      req_options: [plug: fun]
    )
  end

  defp created(conn) do
    conn
    |> Plug.Conn.put_resp_content_type("application/json")
    |> Plug.Conn.send_resp(
      201,
      Jason.encode!(%{
        "id" => 4242,
        "slug" => "kuiper-forge",
        "name" => "Kuiper Forge",
        "owner" => %{"login" => "AstroHQ"},
        "html_url" => "https://github.com/apps/kuiper-forge",
        "webhook_secret" => "whs",
        "pem" => "-----BEGIN RSA PRIVATE KEY-----\nMII\n-----END RSA PRIVATE KEY-----\n"
      })
    )
  end

  test "both pages need a login" do
    assert {:error, {:redirect, %{to: "/admin/login"}}} =
             live(build_conn(), ~p"/admin/forges/github/new")

    conn = get(build_conn(), ~p"/admin/forges/github/callback?code=abc&state=x")
    assert redirected_to(conn, 303) == "/admin/login"
  end

  test "the form posts the manifest to the org's new-App page with a signed state", %{
    conn: conn,
    alice: alice
  } do
    {:ok, view, html} = live(conn, ~p"/admin/forges/github/new")
    assert html =~ "Create GitHub App"

    view
    |> form("#github-app-form",
      app: %{
        name: "Kuiper CI",
        org: "AstroHQ",
        base_url: "https://ci.example.com/",
        webhook: "true"
      }
    )
    |> render_submit()

    assert has_element?(view, ~s|#github-manifest-post[phx-trigger-action]|)

    doc = view |> element("#github-manifest-post") |> render() |> LazyHTML.from_fragment()
    [url] = doc |> LazyHTML.query("form") |> LazyHTML.attribute("action")
    [manifest] = doc |> LazyHTML.query("input[name=manifest]") |> LazyHTML.attribute("value")
    uri = URI.parse(url)
    assert uri.host == "github.com"
    assert uri.path == "/organizations/AstroHQ/settings/apps/new"
    %{"state" => state} = URI.decode_query(uri.query)

    assert {:ok, %{"admin" => id}} =
             GitHubManifest.verify_state(KuiperCoordinatorWeb.Endpoint, state)

    assert id == alice.id

    m = Jason.decode!(manifest)
    assert m["name"] == "Kuiper CI"
    assert m["redirect_url"] == "https://ci.example.com/admin/forges/github/callback"
    assert m["hook_attributes"] == %{"url" => "https://ci.example.com/webhook", "active" => true}
    assert m["default_events"] == ["workflow_job"]
    assert m["default_permissions"]["administration"] == "write"
  end

  test "the manifest's webhook URL follows the stored webhook path", %{conn: conn} do
    {:ok, _} = KuiperCoordinator.Forges.update_settings(%{webhook_path: "/hooks/github"})
    {:ok, view, _html} = live(conn, ~p"/admin/forges/github/new")

    view
    |> form("#github-app-form",
      app: %{name: "Kuiper CI", org: "", base_url: "https://ci.example.com", webhook: "true"}
    )
    |> render_submit()

    doc = view |> element("#github-manifest-post") |> render() |> LazyHTML.from_fragment()
    [manifest] = doc |> LazyHTML.query("input[name=manifest]") |> LazyHTML.attribute("value")

    assert Jason.decode!(manifest)["hook_attributes"]["url"] ==
             "https://ci.example.com/hooks/github"
  end

  test "a bad base URL is refused, not posted", %{conn: conn} do
    {:ok, view, _} = live(conn, ~p"/admin/forges/github/new")

    html =
      view
      |> form("#github-app-form", app: %{name: "x", org: "", base_url: "ftp://nope"})
      |> render_submit()

    assert html =~ "must be an http(s) URL"
    refute has_element?(view, "#github-manifest-post")
  end

  test "callback exchanges the code, saves the App and sends the admin to install it", %{
    conn: conn,
    alice: alice
  } do
    test_pid = self()

    stub_github(fn c ->
      send(test_pid, {:github, c.method, c.request_path})
      created(c)
    end)

    Process.put(:test_pid, test_pid)
    state = GitHubManifest.sign_state(KuiperCoordinatorWeb.Endpoint, %{"admin" => alice.id})
    conn = get(conn, ~p"/admin/forges/github/callback?#{%{code: "abc123", state: state}}")

    assert redirected_to(conn) == "https://github.com/apps/kuiper-forge/installations/new"
    assert_received {:github, "POST", "/app-manifests/abc123/conversions"}
    assert_received {:saved, %{app_id: 4242, webhook_secret: "whs", private_key: pem}, "alice"}
    assert pem =~ "BEGIN RSA PRIVATE KEY"
  end

  # the session can run out while the admin is on GitHub; the code must survive the login
  test "a callback that arrives logged out carries on after the login", %{alice: alice} do
    test_pid = self()
    stub_github(&created/1)
    Process.put(:test_pid, test_pid)
    state = GitHubManifest.sign_state(KuiperCoordinatorWeb.Endpoint, %{"admin" => alice.id})

    conn = get(build_conn(), ~p"/admin/forges/github/callback?#{%{code: "abc123", state: state}}")
    assert redirected_to(conn, 303) == "/admin/login"
    refute_received {:saved, _, _}

    conn =
      conn
      |> recycle()
      |> post(~p"/admin/login", %{"username" => "alice", "password" => "correct horse"})

    conn = get(recycle(conn), redirected_to(conn, 303))
    assert redirected_to(conn) == "https://github.com/apps/kuiper-forge/installations/new"
    assert_received {:saved, %{app_id: 4242}, "alice"}
  end

  test "callback refuses a forged, foreign or missing state without calling GitHub", %{
    conn: conn
  } do
    test_pid = self()
    stub_github(fn c -> send(test_pid, :called) && created(c) end)
    other = GitHubManifest.sign_state(KuiperCoordinatorWeb.Endpoint, %{"admin" => -1})

    for state <- ["forged", other, nil] do
      params = if state, do: %{code: "abc", state: state}, else: %{code: "abc"}
      conn = get(conn, ~p"/admin/forges/github/callback?#{params}")
      assert redirected_to(conn) == "/admin/forges/github/new"
      assert Phoenix.Flash.get(conn.assigns.flash, :error) =~ "start again"
    end

    refute_received :called
    refute_received {:saved, _, _}
  end

  test "a used-up code or a save error shows an error, not a crash", %{conn: conn, alice: alice} do
    state = GitHubManifest.sign_state(KuiperCoordinatorWeb.Endpoint, %{"admin" => alice.id})

    stub_github(fn c ->
      c
      |> Plug.Conn.put_resp_content_type("application/json")
      |> Plug.Conn.send_resp(404, ~s({"message":"Not Found"}))
    end)

    conn1 = get(conn, ~p"/admin/forges/github/callback?#{%{code: "gone", state: state}}")
    assert redirected_to(conn1) == "/admin/forges/github/new"
    assert Phoenix.Flash.get(conn1.assigns.flash, :error) =~ "GitHub"

    stub_github(&created/1)
    Application.put_env(:kuiper_coordinator, :test_sink_result, {:error, :duplicate})
    conn2 = get(conn, ~p"/admin/forges/github/callback?#{%{code: "abc", state: state}}")
    assert redirected_to(conn2) == "/admin/forges/github/new"
    assert Phoenix.Flash.get(conn2.assigns.flash, :error) =~ "save"
  end
end
