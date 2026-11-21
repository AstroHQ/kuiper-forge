defmodule KuiperCoordinatorWeb.Admin.AgentLiveTest do
  @moduledoc """
  Agent detail and agent logs pages (parity UI4, UI5): 404 for unknown agents, registration and
  live limits with split labels, only this agent's runners, revoke, the availability switch
  persisting, logs oldest first with a level filter, `before` paging at 500, and live tail.
  """
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log
  import Phoenix.LiveViewTest

  alias Cirunner.Agent.{AgentStatus, CapacityLimit, LabelSet}
  alias KuiperCoordinator.{Admin, AgentRegistry, Agents, Repo, Runners}
  alias KuiperCoordinator.Agents.LogLine

  setup %{conn: conn} do
    old = Application.get_env(:kuiper_coordinator, :admin)

    Application.put_env(:kuiper_coordinator, :admin,
      enabled: true,
      session_timeout_secs: 3600,
      coordinator_url: "https://ci.example:9543"
    )

    on_exit(fn -> Application.put_env(:kuiper_coordinator, :admin, old || []) end)
    Admin.LoginThrottle.reset()
    {:ok, _} = Admin.create_user("alice", "correct horse", "correct horse")
    conn = post(conn, ~p"/admin/login", %{"username" => "alice", "password" => "correct horse"})
    %{conn: recycle(conn)}
  end

  defp agent!(id, attrs \\ %{}) do
    Repo.insert!(
      struct(
        %Agents.Agent{
          agent_id: id,
          hostname: "#{id}.local",
          agent_type: "tart",
          max_vms: 2,
          serial_number: id,
          expires_at: DateTime.add(DateTime.utc_now(), 86_400, :second)
        },
        attrs
      )
    )
  end

  defp connect!(id, status) do
    test = self()

    pid =
      spawn_link(fn ->
        :ok =
          AgentRegistry.register(id, %{
            hostname: id,
            agent_type: "tart",
            status: status,
            vm_names: []
          })

        send(test, :registered)
        receive do: (:stop -> :ok)
      end)

    assert_receive :registered
    on_exit(fn -> send(pid, :stop) end)
  end

  defp runner!(name, agent_id, attrs \\ %{}) do
    {:ok, _} =
      Runners.add_runner(
        Map.merge(
          %{
            runner_name: name,
            agent_id: agent_id,
            vm_name: name,
            runner_scope: {:org, "astro"},
            forge: "github"
          },
          attrs
        )
      )
  end

  defp log!(agent_id, ts, level, message) do
    Repo.insert!(%LogLine{
      agent_id: agent_id,
      ts: ts,
      seq: 0,
      level: level,
      target: "agent",
      message: message
    })
  end

  describe "agent detail" do
    test "unknown agent is a 404", %{conn: conn} do
      assert_error_sent 404, fn -> get(conn, ~p"/admin/agents/agent_nope") end
    end

    test "shows registration, live limits, split labels and only its runners", %{conn: conn} do
      agent!("agent_mac", %{agent_version: "0.9.1"})
      agent!("agent_other")

      connect!("agent_mac", %AgentStatus{
        labels: ["self-hosted", "macos"],
        label_sets: [
          %LabelSet{labels: ["self-hosted", "macos"], is_default: true},
          %LabelSet{labels: ["self-hosted", "macos", "sequoia"]}
        ],
        max_vms: 2,
        limits: [
          %CapacityLimit{name: "total", max: 5, external: 1, active: 1},
          %CapacityLimit{name: "macos", max: 2, external: 0, active: 1}
        ]
      })

      runner!("mac-0001", "agent_mac", %{
        job_name: "build",
        repository: "astro/luna",
        workflow_name: "CI"
      })

      runner!("other-0001", "agent_other")

      {:ok, view, html} = live(conn, ~p"/admin/agents/agent_mac")
      assert html =~ "agent_mac.local"
      assert html =~ "0.9.1"
      assert has_element?(view, "#agent-status", "Online")
      assert has_element?(view, "#agent-capacity", "2/5")
      assert has_element?(view, "#agent-limits", "macOS")
      assert has_element?(view, "#agent-labels", "sequoia")
      assert has_element?(view, "#runners #runner-mac-0001", "astro/luna")
      refute has_element?(view, "#runner-other-0001")
      assert has_element?(view, ~s|a[href="/admin/agents/agent_mac/logs"]|)
    end

    test "dashboard links each agent to its page", %{conn: conn} do
      agent!("agent_mac")
      {:ok, view, _} = live(conn, ~p"/admin/dashboard")
      assert has_element?(view, ~s|#agent-agent_mac a[href="/admin/agents/agent_mac"]|)
    end

    test "availability switch persists across a reload", %{conn: conn} do
      agent!("agent_mac")
      {:ok, view, _} = live(conn, ~p"/admin/agents/agent_mac")
      refute has_element?(view, "#agent-status", "Unavailable")
      view |> element("#available-agent_mac") |> render_click()
      assert Agents.get_agent("agent_mac").unavailable_at
      {:ok, view, _} = live(conn, ~p"/admin/agents/agent_mac")
      assert has_element?(view, "#agent-status", "Unavailable")
      view |> element("#available-agent_mac") |> render_click()
      refute Agents.get_agent("agent_mac").unavailable_at
    end

    test "revoke marks the agent revoked and goes back to the dashboard", %{conn: conn} do
      agent!("agent_mac")
      {:ok, view, _} = live(conn, ~p"/admin/agents/agent_mac")

      assert {:error, {:live_redirect, %{to: "/admin/dashboard"}}} =
               view |> element("#revoke") |> render_click()

      assert Agents.get_agent("agent_mac").revoked_at
      {:ok, view, _} = live(conn, ~p"/admin/agents/agent_mac")
      assert has_element?(view, "#agent-status", "Revoked")
      refute has_element?(view, "#revoke")
    end
  end

  describe "agent logs" do
    test "unknown agent is a 404", %{conn: conn} do
      assert_error_sent 404, fn -> get(conn, ~p"/admin/agents/agent_nope/logs") end
    end

    test "oldest first, with a level filter", %{conn: conn} do
      agent!("agent_mac")
      log!("agent_mac", 1_000, 3, "first info")
      log!("agent_mac", 2_000, 1, "then an error")
      log!("agent_mac", 3_000, 3, "last info")

      {:ok, _view, html} = live(conn, ~p"/admin/agents/agent_mac/logs")
      [_, a, b, c] = Regex.run(~r/(first info).*(then an error).*(last info)/s, html)
      assert {a, b, c} == {"first info", "then an error", "last info"}

      {:ok, view, html} = live(conn, ~p"/admin/agents/agent_mac/logs?level=error")
      assert html =~ "then an error"
      refute html =~ "first info"
      assert has_element?(view, "#level option[selected][value=error]")
    end

    test "malformed level and before params are ignored (F15)", %{conn: conn} do
      agent!("agent_mac")
      log!("agent_mac", 1_000, 3, "first info")

      for query <- ["before=99999999999999999999999_1", "before[]=1_1", "level[]=x"] do
        {:ok, _view, html} = live(conn, "/admin/agents/agent_mac/logs?" <> query)
        assert html =~ "first info", query
      end
    end

    test "pages 500 at a time with a before cursor", %{conn: conn} do
      agent!("agent_mac")
      for i <- 1..501, do: log!("agent_mac", i * 10, 3, "line #{i}-end")

      {:ok, view, html} = live(conn, ~p"/admin/agents/agent_mac/logs")
      refute html =~ "line 1-end"
      assert html =~ "line 2-end"
      assert html =~ "line 501-end"

      href =
        view
        |> element("#older-logs")
        |> render()
        |> then(&Regex.run(~r/href="([^"]+)"/, &1))
        |> List.last()

      {:ok, view, html} = live(conn, String.replace(href, "&amp;", "&"))
      assert html =~ "line 1-end"
      refute html =~ "line 2-end"
      refute has_element?(view, "#older-logs")
      # live tail only on the newest page
      refute has_element?(view, "#live")
    end

    test "live tail picks up new lines", %{conn: conn} do
      agent!("agent_mac")
      log!("agent_mac", 1_000, 3, "old line")
      {:ok, view, _} = live(conn, ~p"/admin/agents/agent_mac/logs?live=1")
      log!("agent_mac", 2_000, 3, "new line")
      send(view.pid, :tail)
      assert render(view) =~ "new line"
    end
  end
end
