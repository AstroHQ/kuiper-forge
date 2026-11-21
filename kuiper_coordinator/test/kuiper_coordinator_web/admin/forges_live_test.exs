defmodule KuiperCoordinatorWeb.Admin.ForgesLiveTest do
  @moduledoc """
  The Forges pages (D39-D41): list the stored forges with the fleet's state, add a Forgejo
  forge, edit one without its secrets ever coming back to the browser, remove one; and the
  manifest flow's default sink stores the new GitHub App.
  """
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log
  import Phoenix.LiveViewTest

  alias KuiperCoordinator.{Admin, Forges}
  alias KuiperCoordinator.Forges.GitHubAppSink

  @pem "-----BEGIN RSA PRIVATE KEY-----\nMIIsecretkeymaterial\n-----END RSA PRIVATE KEY-----\n"

  setup %{conn: conn} do
    old_admin = Application.get_env(:kuiper_coordinator, :admin)
    Application.put_env(:kuiper_coordinator, :admin, enabled: true, session_timeout_secs: 3600)
    on_exit(fn -> Application.put_env(:kuiper_coordinator, :admin, old_admin || []) end)

    Admin.LoginThrottle.reset()
    {:ok, _} = Admin.create_user("alice", "correct horse", "correct horse")
    conn = post(conn, ~p"/admin/login", %{"username" => "alice", "password" => "correct horse"})
    %{conn: recycle(conn)}
  end

  defp github!(attrs \\ %{}) do
    {:ok, f} =
      Forges.create_forge(
        Map.merge(
          %{
            name: "astro",
            type: :github,
            app_id: 4242,
            private_key: @pem,
            webhook_secret: "whs1"
          },
          attrs
        )
      )

    f
  end

  test "needs a signed-in admin", %{conn: _} do
    conn = build_conn()
    assert {:error, {:redirect, %{to: "/admin/login"}}} = live(conn, ~p"/admin/forges")
  end

  test "lists forges without secrets, with the fleet's state", %{conn: conn} do
    github!()

    {:ok, _} =
      Forges.create_forge(%{
        name: "knope",
        type: :forgejo,
        url: "https://git.example.dev",
        token: "fjtoken-secret",
        scopes: ["lodge"]
      })

    {:ok, view, html} = live(conn, ~p"/admin/forges")
    assert html =~ "astro"
    assert html =~ "4242"
    assert html =~ "knope"
    assert html =~ "https://git.example.dev"
    assert html =~ "lodge"
    refute html =~ "MIIsecretkeymaterial"
    refute html =~ "fjtoken-secret"
    refute html =~ "whs1"
    assert has_element?(view, "#forges-nav-link.btn-active") or html =~ "Forges"
    # default settings are fixed capacity with no runner scope: the reason is shown
    assert has_element?(view, "#fleet-config-error", "runner scope")
  end

  test "the nav has Forges", %{conn: conn} do
    {:ok, view, _} = live(conn, ~p"/admin/dashboard")
    assert has_element?(view, ~s|#admin-nav a[href="/admin/forges"]|, "Forges")
  end

  test "empty state links to both add flows", %{conn: conn} do
    {:ok, view, html} = live(conn, ~p"/admin/forges")
    assert html =~ "No forges yet"
    assert has_element?(view, ~s|a[href="/admin/forges/github/new"]|)
    assert has_element?(view, ~s|a[href="/admin/forges/forgejo/new"]|)
  end

  test "adds a Forgejo forge, with field errors shown", %{conn: conn} do
    Forges.subscribe()
    {:ok, view, _} = live(conn, ~p"/admin/forges/forgejo/new")

    html =
      view
      |> form("#forge-form",
        forge: %{name: "bad name", url: "ftp://x", token: "", scopes: ""}
      )
      |> render_submit()

    assert html =~ "can&#39;t contain"
    assert html =~ "must be an http(s) URL"
    assert Forges.list_forges() == []

    view
    |> form("#forge-form",
      forge: %{
        name: "knope",
        url: "https://git.example.dev/",
        token: " fj ",
        scopes: "lodge\njfro/repo, @jfro",
        webhook_secret: "hooksecret"
      }
    )
    |> render_submit()

    assert_redirect(view, ~p"/admin/forges")
    assert [f] = Forges.list_forges()
    assert f.type == :forgejo
    assert f.url == "https://git.example.dev"
    assert f.token == "fj"
    assert f.scopes == ["lodge", "jfro/repo", "@jfro"]
    assert f.webhook_secret == "hooksecret"
    assert_receive {:forges_changed}
  end

  test "a failed live check shows the forge's message", %{conn: conn} do
    old = Application.get_env(:kuiper_coordinator, :validate_forges_on_save)
    Application.put_env(:kuiper_coordinator, :validate_forges_on_save, true)

    Application.put_env(:kuiper_coordinator, :forge_req_options,
      plug: fn conn -> Plug.Conn.send_resp(conn, 401, ~s({"message":"token is invalid"})) end
    )

    on_exit(fn ->
      Application.put_env(:kuiper_coordinator, :validate_forges_on_save, old)
      Application.delete_env(:kuiper_coordinator, :forge_req_options)
    end)

    {:ok, view, _} = live(conn, ~p"/admin/forges/forgejo/new")

    html =
      view
      |> form("#forge-form",
        forge: %{name: "knope", url: "https://git.example.dev", token: "t", scopes: "lodge"}
      )
      |> render_submit()

    assert html =~ ~s(id="forge-base-error")
    assert html =~ "token is invalid"
    assert Forges.list_forges() == []
  end

  test "editing never fills secrets back in, and blank keeps them", %{conn: conn} do
    f = github!()
    {:ok, view, html} = live(conn, ~p"/admin/forges/#{f.id}/edit")
    refute html =~ "MIIsecretkeymaterial"
    refute html =~ "whs1"
    assert html =~ "leave blank to keep"
    # the type can't be changed, so there's no type field
    refute has_element?(view, ~s|[name="forge[type]"]|)

    view
    |> form("#forge-form", forge: %{name: "astro-gh", app_id: "4343", private_key: ""})
    |> render_submit()

    assert_redirect(view, ~p"/admin/forges")
    f = Forges.get_forge!(f.id)
    assert f.name == "astro-gh"
    assert f.app_id == 4343
    assert f.private_key == @pem
    assert f.webhook_secret == "whs1"
  end

  test "removes a forge", %{conn: conn} do
    f = github!()
    {:ok, view, _} = live(conn, ~p"/admin/forges")
    view |> element("#delete-forge-#{f.id}") |> render_click()
    assert Forges.list_forges() == []
    refute has_element?(view, "#forge-#{f.id}")
  end

  test "an unknown forge id goes back to the list", %{conn: conn} do
    assert {:error, {:live_redirect, %{to: "/admin/forges"}}} =
             live(conn, ~p"/admin/forges/999999/edit")
  end

  describe "the manifest flow's default sink" do
    @app %{
      app_id: 4242,
      slug: "kuiper-forge",
      name: "Kuiper Forge",
      owner: "AstroHQ",
      html_url: "https://github.com/apps/kuiper-forge",
      private_key: @pem,
      webhook_secret: "whs",
      client_id: nil,
      client_secret: nil
    }

    test "stores the App as a GitHub forge without a live check (it has no install yet)" do
      Application.put_env(:kuiper_coordinator, :validate_forges_on_save, true)
      on_exit(fn -> Application.put_env(:kuiper_coordinator, :validate_forges_on_save, false) end)

      assert GitHubAppSink.impl() == GitHubAppSink
      assert {:ok, f} = GitHubAppSink.save_github_app(@app, "alice")
      assert %{name: "kuiper-forge", type: :github, app_id: 4242} = f
      assert Forges.get_forge!(f.id).private_key == @pem
      assert Forges.get_forge!(f.id).webhook_secret == "whs"
    end

    test "a taken name gets the App id appended" do
      github!(%{name: "kuiper-forge", app_id: 1})
      assert {:ok, %{name: "kuiper-forge-4242"}} = GitHubAppSink.save_github_app(@app, "alice")
    end
  end
end
