defmodule KuiperCoordinator.Fleet.ManagerTest do
  use KuiperCoordinator.DataCase, async: false
  import Mox

  alias KuiperCoordinator.{AgentDispatchMock, ForgeMock, Jobs}
  alias KuiperCoordinator.Fleet.Manager

  setup :set_mox_global
  setup :verify_on_exit!

  @scope {:org, "astro"}
  @forges %{"github" => {ForgeMock, :client}}

  defp start(opts) do
    opts = Keyword.merge([name: nil, tick_ms: 60_000, forges: @forges, async: false], opts)
    pid = start_supervised!({Manager, opts})
    Ecto.Adapters.SQL.Sandbox.allow(KuiperCoordinator.Repo, self(), pid)
    pid
  end

  test "fixed capacity reconciles the pools on each tick (P1)" do
    test = self()

    expect(AgentDispatchMock, :pool_definitions, 2, fn ->
      send(test, :reconciled)
      []
    end)

    pid = start(mode: :fixed_capacity, runner_scope: @scope)
    send(pid, :tick)
    assert_receive :reconciled
    send(pid, :tick)
    assert_receive :reconciled
  end

  test "fixed capacity with no runner scope creates nothing" do
    pid = start(mode: :fixed_capacity, runner_scope: nil)
    send(pid, :tick)
    # no pool_definitions expectation: Mox fails the test if it's called
    assert %{mode: :fixed_capacity} = :sys.get_state(pid)
  end

  test "webhook mode processes pending jobs on a new job, a reprocess and a tick (P6, W7)" do
    {:ok, _} =
      Jobs.add_pending(%{job_key: "github:1", forge: "github", runner_scope: @scope})

    test = self()

    expect(AgentDispatchMock, :select_agent, 3, fn _, _ ->
      send(test, :processed)
      {:error, :no_available_agent}
    end)

    pid = start(mode: :webhook)
    Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "fleet", {:new_job, "github:1"})
    assert_receive :processed
    Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "fleet", :reprocess)
    assert_receive :processed
    send(pid, :tick)
    assert_receive :processed
    # unrelated fleet events don't trigger a pass
    Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "fleet", {:job_completed, "github:1"})
    :sys.get_state(pid)
    refute_received :processed
  end

  test "a crashing pass doesn't take the manager down" do
    expect(AgentDispatchMock, :pool_definitions, fn -> raise "boom" end)
    pid = start(mode: :fixed_capacity, runner_scope: @scope)
    send(pid, :tick)
    assert %{mode: :fixed_capacity} = :sys.get_state(pid)
    assert Process.alive?(pid)
  end

  test "an unknown mode is refused at start" do
    assert {:error, {:unknown_mode, :polling_v2}} =
             Manager.init(mode: :polling_v2, forges: @forges, tick_ms: 1)
  end
end
