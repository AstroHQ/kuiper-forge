defmodule KuiperCoordinatorWeb.ForgeFormLogTest do
  # D79 (Nye F23): saving a forge from the admin form must not print its
  # secrets in the debug log either. LiveView logs each event's params at debug,
  # filtered only by :phoenix, :filter_parameters (default ["password"]).
  use KuiperCoordinatorWeb.ConnCase, async: false
  import Phoenix.LiveViewTest
  import ExUnit.CaptureLog
  alias KuiperCoordinator.{Admin, Forges}

  setup %{conn: conn} do
    old_admin = Application.get_env(:kuiper_coordinator, :admin)
    Application.put_env(:kuiper_coordinator, :admin, session_timeout_secs: 3600)
    level = Logger.level()

    on_exit(fn ->
      Application.put_env(:kuiper_coordinator, :admin, old_admin || [])
      Logger.configure(level: level)
    end)

    Admin.LoginThrottle.reset()
    {:ok, _} = Admin.create_user("alice", "correct horse", "correct horse")
    conn = post(conn, ~p"/admin/login", %{"username" => "alice", "password" => "correct horse"})
    %{conn: recycle(conn)}
  end

  test "rotating a forge's secrets from the admin form keeps them out of the debug log", %{
    conn: conn
  } do
    {:ok, fj} =
      Forges.create_forge(
        %{
          name: "knope",
          type: :forgejo,
          url: "https://git.example.dev",
          token: "fjtoken-old",
          scopes: ["lodge"]
        },
        validate: false
      )

    {:ok, view, _} = live(conn, ~p"/admin/forges/#{fj.id}/edit")
    Logger.configure(level: :debug)

    log =
      capture_log([level: :debug], fn ->
        view
        |> form("#forge-form",
          forge: %{token: "fj-rotated-token-77q", webhook_secret: "hook-rotated-88w"}
        )
        |> render_submit()
      end)

    Logger.configure(level: :warning)
    assert Forges.get_forge_by_name("knope").token == "fj-rotated-token-77q"
    assert log =~ "HANDLE EVENT", "debug LiveView log wasn't captured"
    refute log =~ "fj-rotated-token-77q", "debug log has the Forgejo token"
    refute log =~ "hook-rotated-88w", "debug log has the webhook secret"
  end

  test "rotating a GitHub forge's private key from the admin form keeps it out of the debug log",
       %{conn: conn} do
    pem =
      "-----BEGIN RSA PRIVATE KEY-----\nMIInyeRotatedKeyBody55k\n-----END RSA PRIVATE KEY-----\n"

    {:ok, gh} =
      Forges.create_forge(
        %{
          name: "astro",
          type: :github,
          app_id: 4242,
          private_key: "-----BEGIN RSA PRIVATE KEY-----\nold\n-----END RSA PRIVATE KEY-----\n"
        },
        validate: false
      )

    {:ok, view, _} = live(conn, ~p"/admin/forges/#{gh.id}/edit")
    Logger.configure(level: :debug)

    log =
      capture_log([level: :debug], fn ->
        view |> form("#forge-form", forge: %{private_key: pem}) |> render_submit()
      end)

    Logger.configure(level: :warning)

    assert log =~ "HANDLE EVENT", "debug LiveView log wasn't captured"
    refute log =~ "MIInyeRotatedKeyBody55k", "debug log has the GitHub App private key"
  end
end
