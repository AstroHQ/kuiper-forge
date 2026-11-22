defmodule KuiperCoordinatorWeb.Admin.MetricsLiveTest do
  @moduledoc "Metrics page (parity UI6, #67) and the agent page's 7-day card."
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log
  import Phoenix.LiveViewTest

  alias KuiperCoordinator.{Admin, Agents, Repo, RunHistory}
  alias KuiperCoordinatorWeb.Admin.MetricsLive

  setup %{conn: conn} do
    old = Application.get_env(:kuiper_coordinator, :admin)
    Application.put_env(:kuiper_coordinator, :admin, enabled: true, session_timeout_secs: 3600)
    on_exit(fn -> Application.put_env(:kuiper_coordinator, :admin, old || []) end)
    Admin.LoginThrottle.reset()
    {:ok, _} = Admin.create_user("alice", "correct horse", "correct horse")
    conn = post(conn, ~p"/admin/login", %{"username" => "alice", "password" => "correct horse"})

    Repo.insert!(%Agents.Agent{
      agent_id: "agent_mac",
      hostname: "mac.local",
      agent_type: "tart",
      max_vms: 2,
      serial_number: "s1",
      expires_at: DateTime.add(DateTime.utc_now(), 86_400, :second)
    })

    %{conn: recycle(conn)}
  end

  defp seed do
    now = System.system_time(:millisecond)

    for {outcome, i} <- Enum.with_index(~w(completed completed completed failed)) do
      RunHistory.insert_run!(%{
        runner_name: "mac-#{i}",
        agent_id: "agent_mac",
        hostname: "mac.local",
        agent_type: "tart",
        image: "</script><b>macos-15</b>",
        labels: ["macos"],
        requested_at: now - 3_600_000 + i,
        started_at: now - 3_600_000 + i + 48_000,
        finished_at: now - 1_000,
        outcome: outcome,
        failed_phase: if(outcome == "failed", do: "boot"),
        install_source: "hit",
        steps: %{"boot" => 20_000, "install" => 3_000, "job" => 60_000}
      })
    end

    RunHistory.insert_run!(%{
      runner_name: "old-0",
      agent_id: "agent_mac",
      hostname: "mac.local",
      agent_type: "tart",
      requested_at: now - 20 * 86_400_000,
      outcome: "failed"
    })

    RunHistory.insert_job!(%{
      job_key: "github:1",
      forge: "github",
      queued_at: now - 3_600_000 - 2_000,
      seen_at: now - 3_600_000,
      started_at: now - 3_600_000 + 50_000,
      runner_name: "mac-0",
      attempts: 1,
      outcome: "ran"
    })
  end

  test "empty history says so", %{conn: conn} do
    {:ok, _view, html} = live(conn, ~p"/admin/metrics")
    assert html =~ "No runners in the last 7d"
  end

  test "header stats, agent and image tables, installs and jobs", %{conn: conn} do
    seed()
    {:ok, view, html} = live(conn, ~p"/admin/metrics")

    assert has_element?(view, "#stat-runs", "4")
    assert has_element?(view, "#stat-success", "75.0%")
    assert has_element?(view, "#stat-success", "1 failed")
    assert has_element?(view, "#stat-provision", "48.0s")
    assert has_element?(view, "#stat-wait", "52.0s")
    assert has_element?(view, "#agents a[href='/admin/agents/agent_mac']", "mac.local")
    assert has_element?(view, "#agents", "boot 1")
    assert has_element?(view, "#images", "macos-15")
    assert has_element?(view, "#installs", "Cache hit")
    assert has_element?(view, "#jobs", "1 jobs")
    # names are escaped wherever they land, the chart data included
    refute html =~ "<b>macos-15</b>"
    assert has_element?(view, "[phx-hook=MetricsCharts][data-charts]")
  end

  test "range tabs pick the window; an unknown range is 7d", %{conn: conn} do
    seed()
    {:ok, view, _} = live(conn, ~p"/admin/metrics?range=30d")
    assert has_element?(view, "#stat-runs", "5")
    assert has_element?(view, "a[aria-selected='true']", "30d")

    {:ok, view, _} = live(conn, ~p"/admin/metrics?range=bogus")
    assert has_element?(view, "a[aria-selected='true']", "7d")
    assert has_element?(view, "#stat-runs", "4")
  end

  test "the agent page shows its last 7 days", %{conn: conn} do
    {:ok, view, _} = live(conn, ~p"/admin/agents/agent_mac")
    refute has_element?(view, "#performance")

    seed()
    {:ok, view, _} = live(conn, ~p"/admin/agents/agent_mac")
    assert has_element?(view, "#performance", "Last 7 days")
    assert has_element?(view, "#performance .badge-error", "75.0%")
    assert has_element?(view, "#performance", "1 failed: boot 1")
    assert has_element?(view, "#performance", "48.0s")
  end

  test "success status thresholds" do
    assert MetricsLive.success_status(nil) == :idle
    assert MetricsLive.success_status(95.0) == :ok
    assert MetricsLive.success_status(80.0) == :warn
    assert MetricsLive.success_status(79.9) == :bad
  end

  test "metrics needs a login" do
    conn = build_conn()
    assert {:error, {:redirect, %{to: "/admin/login"}}} = live(conn, ~p"/admin/metrics")
  end
end
