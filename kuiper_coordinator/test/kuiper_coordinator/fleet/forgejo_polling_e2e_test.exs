defmodule KuiperCoordinator.Fleet.ForgejoPollingE2ETest do
  # Nye's Forgejo read (Nov 21), item 6 (parity FJ5, J5): end to end through the fleet manager
  # in polling mode with the real Forgejo client against a fake Forgejo. The same job listed in
  # an org scope and the instance scope becomes one pending job and gets exactly one runner,
  # registered in the first scope that listed it.
  use KuiperCoordinator.DataCase, async: false
  import Mox

  alias Cirunner.Agent.{CreateRunnerCommand, ForgejoRunner}
  alias KuiperCoordinator.{AgentDispatchMock, Jobs, Runners}
  alias KuiperCoordinator.Fleet.Manager
  alias KuiperCoordinator.Forgejo.Client

  setup :set_mox_global
  setup :verify_on_exit!

  @job %{
    "id" => 11,
    "handle" => "h-dup",
    "repo_id" => 7,
    "runs_on" => ["self-hosted", "linux"],
    "status" => "waiting",
    "name" => "build"
  }

  defp fake_forgejo do
    test_pid = self()
    name = {__MODULE__, make_ref()}

    Req.Test.stub(name, fn conn ->
      case {conn.method, conn.path_info} do
        {"GET", ["api", "v1", "orgs", "lodge", "actions", "runners", "jobs"]} ->
          Req.Test.json(conn, [@job])

        {"GET", ["api", "v1", "admin", "actions", "runners", "jobs"]} ->
          Req.Test.json(conn, [@job])

        {"GET", ["api", "v1", "repositories", "7"]} ->
          Req.Test.json(conn, %{"full_name" => "lodge/app"})

        {"POST", ["api", "v1" | path]} ->
          send(test_pid, {:registered, Enum.join(path, "/")})
          Req.Test.json(conn, %{"id" => 1, "uuid" => "u-1", "token" => "tok-1"})
      end
    end)

    {:ok, c} =
      Client.new(
        url: "http://forgejo.test",
        token: "t",
        scopes: ["lodge", "*"],
        req_options: [plug: {Req.Test, name}, retry: false]
      )

    {Client, c}
  end

  test "a job listed in an org and the instance scope gets one runner" do
    labels = ["self-hosted", "linux"]
    expect(AgentDispatchMock, :select_agent, 1, fn ^labels, _ -> {:ok, "a1"} end)
    expect(AgentDispatchMock, :hostname, 1, fn "a1" -> "fj1" end)
    expect(AgentDispatchMock, :reserve_slot, 1, fn ^labels, _name, _ -> {:ok, "a1"} end)

    expect(AgentDispatchMock, :dispatch_create, 1, fn "a1", %CreateRunnerCommand{} = cmd ->
      assert %ForgejoRunner{handle: "h-dup", uuid: "u-1", token: "tok-1"} = cmd.forgejo
      :ok
    end)

    {:ok, pid} =
      Manager.start_link(
        name: nil,
        mode: :polling,
        forges: %{"fj" => fake_forgejo()},
        tick_ms: 3_600_000,
        # every tick polls
        scan_ms: 0,
        async: false,
        webhook: []
      )

    # startup: pass, poll, pass for what the poll recovered
    :sys.get_state(pid)

    assert [%{job_key: "fj:h-dup", runner_scope: {:org, "lodge"}, repository: "lodge/app"}] =
             Jobs.list_pending()

    assert [runner] = Runners.for_job("fj:h-dup")
    assert Runners.get_runner(runner).runner_scope == {:org, "lodge"}
    assert_received {:registered, "orgs/lodge/actions/runners"}
    refute_received {:registered, _}

    # a second poll lists the job again: still one runner, no new registration
    send(pid, :tick)
    :sys.get_state(pid)
    assert [^runner] = Runners.for_job("fj:h-dup")
    refute_received {:registered, _}
    GenServer.stop(pid)
  end
end
