defmodule KuiperCoordinatorWeb.Admin.DashboardLiveTest do
  @moduledoc """
  The admin dashboard page (parity UI2/UI3/#40): counts, a connected agent's capacity and labels,
  pending jobs with their status, creating a registration token shows a kfr1_ bundle that
  decodes to the token and URL, deleting a token, and the availability switch.
  """
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log
  import Phoenix.LiveViewTest

  alias Cirunner.Agent.{AgentStatus, CapacityLimit, LabelSet}
  alias KuiperCoordinator.{Admin, AgentRegistry, Agents, Jobs, Repo}

  setup %{conn: conn} do
    old = Application.get_env(:kuiper_coordinator, :admin)

    Application.put_env(:kuiper_coordinator, :admin,
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

  # a live connection owned by a process that stays up for the test
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

  test "shows counts, capacity, labels and pending job status", %{conn: conn} do
    agent!("agent_mac", %{agent_version: "0.9.1"})
    agent!("agent_off")

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

    {:ok, :inserted} =
      Jobs.add_pending(%{
        job_key: "github:77",
        forge: "github",
        job_labels: ["self-hosted", "linux"],
        agent_labels: ["self-hosted", "linux"],
        runner_scope: {:org, "astro"},
        repository: "astro/app",
        workflow_name: "CI",
        job_name: "test",
        queued_at: DateTime.add(DateTime.utc_now(), -90, :second)
      })

    {:ok, view, _html} = live(conn, ~p"/admin/dashboard")

    assert view |> element("#stat-connected") |> render() =~ ">1<"
    assert view |> element("#stat-registered") |> render() =~ ">2<"
    assert view |> element("#stat-pending") |> render() =~ ">1<"
    cap = view |> element("#stat-capacity") |> render()
    assert cap =~ "2" and cap =~ "/ 5"

    mac = view |> element("#agent-agent_mac") |> render()
    assert mac =~ "Online" and mac =~ "0.9.1" and mac =~ "2/5" and mac =~ "(1 external)"
    assert mac =~ "macOS 1/2" and mac =~ "sequoia"
    assert view |> element("#agent-agent_off") |> render() =~ "Offline"

    job = view |> element("#job-github\\:77") |> render()
    assert job =~ "CI / </span>test" and job =~ "astro/app" and job =~ "No matching agent"
  end

  test "creating a token shows a kfr1_ bundle once; deleting removes it", %{conn: conn} do
    {ca_cert, ca_key} = KuiperCoordinator.AgentGrpcCase.fixture_ca()
    KuiperCoordinator.CA.install!(ca_cert, ca_key)
    {:ok, view, html} = live(conn, ~p"/admin/dashboard")
    assert html =~ "No pending tokens"

    html = view |> element("#create-token") |> render_click()
    [_, bundle] = Regex.run(~r/id="register-bundle">(kfr1_[A-Za-z0-9_-]+)</, html)
    [%{token: token, created_by: "alice"}] = Agents.list_registration_tokens()

    json =
      bundle
      |> String.trim_leading("kfr1_")
      |> Base.url_decode64!(padding: false)
      |> Jason.decode!()

    assert json["t"] == token
    assert json["u"] == "https://ci.example:9543"
    assert json["m"] == "ca"

    assert view |> element("#token-#{token} button") |> render_click() =~ "No pending tokens"
    assert Agents.list_registration_tokens() == []
  end

  test "the availability switch marks an agent unavailable and back", %{conn: conn} do
    agent!("agent_a")
    {:ok, view, _} = live(conn, ~p"/admin/dashboard")

    html = view |> element("#available-agent_a") |> render_click()
    assert Agents.get_agent("agent_a").unavailable_at
    assert html =~ "Unavailable"

    html = view |> element("#available-agent_a") |> render_click()
    refute Agents.get_agent("agent_a").unavailable_at
    refute html =~ ">\n                    Unavailable"
  end

  describe "update badge (#75)" do
    setup do
      KuiperCoordinator.AgentUpdates.set(
        KuiperCoordinator.AgentUpdates.newest_agent_releases([
          %{
            "tag_name" => "kuiper-tart-agent-v0.10.0",
            "html_url" =>
              "https://github.com/AstroHQ/kuiper-forge/releases/tag/kuiper-tart-agent-v0.10.0",
            "published_at" => "2026-11-20T10:00:00Z"
          }
        ])
      )

      on_exit(fn -> KuiperCoordinator.AgentUpdates.set(%{}) end)
    end

    test "agents behind the newest release link to it; current ones don't", %{conn: conn} do
      agent!("agent_old", %{agent_version: "0.9.1"})
      agent!("agent_new", %{agent_version: "0.10.0"})
      agent!("agent_unknown")

      {:ok, view, _} = live(conn, ~p"/admin/dashboard")

      assert has_element?(
               view,
               "#agent-agent_old a.update-badge[href$='kuiper-tart-agent-v0.10.0']",
               "0.10.0 available"
             )

      refute has_element?(view, "#agent-agent_new .update-badge")
      refute has_element?(view, "#agent-agent_unknown .update-badge")

      {:ok, view, _} = live(conn, ~p"/admin/agents/agent_old")
      assert has_element?(view, "#agent-version .update-badge", "0.10.0 available")
      {:ok, view, _} = live(conn, ~p"/admin/agents/agent_new")
      refute has_element?(view, "#agent-version .update-badge")
    end
  end
end
