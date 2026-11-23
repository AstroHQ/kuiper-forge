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

  test "an unreadable CA shows the reason inside the modal and keeps the page alive", %{
    conn: conn
  } do
    old = Application.get_env(:kuiper_coordinator, :agent_grpc)

    missing =
      Path.join(System.tmp_dir!(), "kc-no-ca-#{System.unique_integer([:positive])}/ca.crt")

    Application.put_env(:kuiper_coordinator, :agent_grpc,
      ca_cert: missing,
      server_trust_mode: "ca"
    )

    on_exit(fn -> Application.put_env(:kuiper_coordinator, :agent_grpc, old || []) end)

    {:ok, view, _} = live(conn, ~p"/admin/dashboard")
    view |> element("#register-agent") |> render_click()

    error = view |> element("#register-modal #register-error") |> render()
    assert error =~ "Couldn&#39;t build the registration bundle"
    assert error =~ "cannot read server CA certificate"
    refute has_element?(view, "#register-modal #register-token")
    assert Process.alive?(view.pid)
    assert render(view) =~ "No pending tokens"
    assert Agents.list_registration_tokens() == []

    view |> element("#register-close") |> render_click()
    refute has_element?(view, "#register-modal")
  end

  test "Register agent opens a modal with just the kfr1_ bundle to copy", %{conn: conn} do
    {ca_cert, ca_key} = KuiperCoordinator.AgentGrpcCase.fixture_ca()
    KuiperCoordinator.CA.install!(ca_cert, ca_key)
    {:ok, view, html} = live(conn, ~p"/admin/dashboard")
    assert html =~ "No pending tokens"
    refute has_element?(view, "#register-modal")

    view |> element("#register-agent") |> render_click()
    assert has_element?(view, "#register-modal.modal.modal-open")
    assert has_element?(view, "#register-modal", "Copy it now; it won't be shown again")
    # keyboard users land in the dialog and Tab stays there
    assert has_element?(view, "#register-modal #register-box.modal-box[phx-mounted]")
    assert has_element?(view, "#register-box #register-box-start[tabindex]")

    shown = view |> element("#register-modal #register-token") |> render()

    # the bundle alone is what Copy takes: no command around it, no agent assumed
    [_, bundle] = Regex.run(~r/>\s*(kfr1_[A-Za-z0-9_-]+)\s*</, shown)

    # wraps inside the modal instead of squeezing into a column or running off the edge
    assert shown =~ ~r/class="[^"]*break-all/
    assert shown =~ ~r/class="[^"]*whitespace-pre-wrap/
    assert has_element?(view, "#register-copy[phx-hook][data-copy-target='#register-token']")

    # every kind of agent gets an example, with a placeholder where the token goes
    examples = view |> element("#register-examples") |> render()

    for agent <- ~w(tart proxmox libvirt) do
      assert examples =~ "kuiper-#{agent}-agent register &lt;token&gt;"
    end

    refute examples =~ bundle
    # the hook owns the label, so a refresh patch can't undo "Copied"
    assert has_element?(view, "#register-copy #register-copy-label[phx-update='ignore']", "Copy")

    [%{token: token, created_by: "alice"}] = Agents.list_registration_tokens()

    json =
      bundle
      |> String.trim_leading("kfr1_")
      |> Base.url_decode64!(padding: false)
      |> Jason.decode!()

    assert json["t"] == token
    assert json["u"] == "https://ci.example:9543"
    assert json["m"] == "ca"

    # the bundle lives only in the modal, not on the page behind it
    refute view |> element("#registration") |> render() =~ bundle
  end

  test "closing the modal drops the bundle; the token stays pending until deleted", %{conn: conn} do
    {ca_cert, ca_key} = KuiperCoordinator.AgentGrpcCase.fixture_ca()
    KuiperCoordinator.CA.install!(ca_cert, ca_key)
    {:ok, view, _} = live(conn, ~p"/admin/dashboard")

    view |> element("#register-agent") |> render_click()
    shown = view |> element("#register-token") |> render()
    [_, bundle] = Regex.run(~r/(kfr1_[A-Za-z0-9_-]+)/, shown)

    html = view |> element("#register-close") |> render_click()
    refute has_element?(view, "#register-modal")
    refute html =~ bundle
    assert :sys.get_state(view.pid).socket.assigns.new_bundle == nil

    [%{token: token}] = Agents.list_registration_tokens()
    handle = Agents.token_handle(token)
    assert has_element?(view, "#token-#{handle}")

    # reopening makes a fresh token; the old bundle never comes back
    view |> element("#register-agent") |> render_click()
    refute view |> element("#register-token") |> render() =~ bundle

    view |> element("#register-modal") |> render_keydown(%{"key" => "Escape"})
    refute has_element?(view, "#register-modal")

    view |> element("#token-#{handle} button") |> render_click()
    refute has_element?(view, "#token-#{handle}")
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
