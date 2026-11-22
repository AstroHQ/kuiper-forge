defmodule KuiperCoordinatorWeb.Admin.ProvisioningLiveTest do
  @moduledoc """
  The provisioning page (D39): edit the one settings row (mode, scope, timeouts, required
  labels, webhook path and forge) and the label mappings, all stored through `Forges`.
  """
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log
  import Phoenix.LiveViewTest

  alias KuiperCoordinator.{Admin, Forges}

  setup %{conn: conn} do
    old_admin = Application.get_env(:kuiper_coordinator, :admin)
    Application.put_env(:kuiper_coordinator, :admin, enabled: true, session_timeout_secs: 3600)
    on_exit(fn -> Application.put_env(:kuiper_coordinator, :admin, old_admin || []) end)

    Admin.LoginThrottle.reset()
    {:ok, _} = Admin.create_user("alice", "correct horse", "correct horse")
    conn = post(conn, ~p"/admin/login", %{"username" => "alice", "password" => "correct horse"})
    %{conn: recycle(conn)}
  end

  test "needs a signed-in admin" do
    assert {:error, {:redirect, %{to: "/admin/login"}}} =
             live(build_conn(), ~p"/admin/forges/provisioning")
  end

  test "shows the defaults before anything is saved", %{conn: conn} do
    {:ok, view, html} = live(conn, ~p"/admin/forges/provisioning")
    assert html =~ "Provisioning"
    assert has_element?(view, "#settings_idle_runner_timeout_s[value='600']")
    assert has_element?(view, "#settings_required_labels[value='self-hosted']")
  end

  test "saves settings, splitting labels and showing errors", %{conn: conn} do
    {:ok, _} =
      Forges.create_forge(
        %{
          name: "astro",
          type: :github,
          app_id: 1,
          private_key: "-----BEGIN RSA PRIVATE KEY-----\nMIIk\n-----END RSA PRIVATE KEY-----\n",
          webhook_secret: "w"
        },
        validate: false
      )

    Forges.subscribe()
    {:ok, view, _} = live(conn, ~p"/admin/forges/provisioning")

    html =
      view
      |> form("#settings-form", settings: %{poll_interval_s: "3", webhook_path: "hook"})
      |> render_submit()

    assert html =~ "must be greater than or equal to 10"
    assert html =~ "must start with /"
    assert Forges.get_settings().id == 1 and is_nil(Forges.get_settings().inserted_at)

    view
    |> form("#settings-form",
      settings: %{
        mode: "webhook",
        poll_interval_s: "30",
        idle_runner_timeout_s: "60",
        required_labels: "self-hosted, macos ,,arm64",
        webhook_path: "/gh",
        webhook_forge: "astro"
      }
    )
    |> render_submit()

    assert_receive {:forges_changed}
    s = Forges.get_settings()
    assert s.mode == :webhook
    assert s.poll_interval_s == 30
    assert s.idle_runner_timeout_s == 120
    assert s.required_labels == ["self-hosted", "macos", "arm64"]
    assert s.webhook_path == "/gh"
    assert render(view) =~ "Saved provisioning settings"
  end

  test "adds, edits and removes label mappings", %{conn: conn} do
    {:ok, view, _} = live(conn, ~p"/admin/forges/provisioning")

    assert view
           |> form("#mapping-form", mapping: %{labels: " , "})
           |> render_submit() =~ "should have at least 1 item"

    view
    |> form("#mapping-form",
      mapping: %{
        labels: "macos, xcode-26",
        runner_scope: "AstroHQ",
        runner_group: "mac",
        position: "1"
      }
    )
    |> render_submit()

    assert [m] = Forges.list_label_mappings()
    assert m.labels == ["macos", "xcode-26"]
    assert m.runner_scope == "AstroHQ"
    assert has_element?(view, "#mapping-#{m.id}", "macos, xcode-26")

    view |> element("#mapping-#{m.id} button", "Edit") |> render_click()
    assert has_element?(view, "#mapping_labels[value='macos, xcode-26']")

    view |> form("#mapping-form", mapping: %{labels: "linux"}) |> render_submit()
    assert [%{id: id, labels: ["linux"]}] = Forges.list_label_mappings()
    assert id == m.id

    view |> element("#mapping-#{m.id} button", "Remove") |> render_click()
    assert Forges.list_label_mappings() == []
    refute has_element?(view, "#mapping-#{m.id}")
  end

  test "ignores malformed params instead of crashing", %{conn: conn} do
    {:ok, view, _} = live(conn, ~p"/admin/forges/provisioning")
    assert render_submit(view, "save_settings", %{"settings" => "x"}) =~ "Provisioning"

    assert render_submit(view, "save_mapping", %{"mapping" => %{"labels" => ["a"]}}) =~
             "Provisioning"

    assert render_click(view, "edit_mapping", %{"id" => "nope"}) =~ "Provisioning"
    assert render_click(view, "delete_mapping", %{"id" => "999999"}) =~ "Provisioning"
  end
end
