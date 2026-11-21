defmodule KuiperCoordinatorWeb.Admin.ErrorsLiveTest do
  @moduledoc "Errors page (parity UI7, #74) and the agent page's recent failures (#33)."
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log
  import Phoenix.LiveViewTest

  alias KuiperCoordinator.{Admin, AgentFailures, Agents, Repo}

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

    :ok =
      AgentFailures.record(
        "agent_mac",
        :runner_failed,
        "mac-0001",
        "github:42",
        "vm boot timed out"
      )

    :ok = AgentFailures.record("agent_gone", :command_rejected, nil, nil, "at capacity")
    %{conn: recycle(conn)}
  end

  test "lists failures with hostnames, falling back to the id, and 24 h counts", %{conn: conn} do
    {:ok, view, _} = live(conn, ~p"/admin/errors")
    assert has_element?(view, "#failures", "vm boot timed out")
    assert has_element?(view, "#failures", "mac.local")
    assert has_element?(view, "#failures", "agent_gone")
    assert has_element?(view, "#count-agent_mac", "mac.local")
    assert has_element?(view, "#count-totals")
    assert has_element?(view, ~s|#admin-nav a[href="/admin/errors"]|)
  end

  test "filters by kind and agent; an unknown kind shows everything", %{conn: conn} do
    {:ok, view, _} = live(conn, ~p"/admin/errors?kind=command_rejected")
    refute has_element?(view, "#failures", "vm boot timed out")
    assert has_element?(view, "#failures", "at capacity")

    {:ok, view, _} = live(conn, ~p"/admin/errors?agent=agent_mac")
    assert has_element?(view, "#failures", "vm boot timed out")
    refute has_element?(view, "#failures", "at capacity")

    {:ok, view, _} = live(conn, ~p"/admin/errors?kind=bogus")
    assert has_element?(view, "#failures", "at capacity")
    assert has_element?(view, "#failures", "vm boot timed out")

    view |> form("#error-filter", %{kind: "runner_failed", agent: ""}) |> render_change()
    assert_patch(view, "/admin/errors?kind=runner_failed")
    refute has_element?(view, "#failures", "at capacity")
  end

  test "the agent page shows its recent failures", %{conn: conn} do
    {:ok, view, _} = live(conn, ~p"/admin/agents/agent_mac")
    assert has_element?(view, "#agent-failures", "vm boot timed out")
    assert has_element?(view, "#agent-failures", "github:42")
    refute has_element?(view, "#agent-failures", "at capacity")
  end
end
