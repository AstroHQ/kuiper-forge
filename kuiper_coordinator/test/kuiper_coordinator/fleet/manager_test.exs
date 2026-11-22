defmodule KuiperCoordinator.Fleet.ManagerTest do
  use KuiperCoordinator.DataCase, async: false
  import Mox

  alias KuiperCoordinator.{AgentDispatchMock, ForgeMock, Jobs}
  alias KuiperCoordinator.Fleet.Manager
  alias KuiperCoordinator.Forge.QueuedJob

  setup :set_mox_global
  setup :verify_on_exit!

  setup do
    stub(ForgeMock, :runner_kind, fn _ -> "github" end)
    stub(AgentDispatchMock, :unavailable_agents, fn -> [] end)
    :ok
  end

  @scope {:org, "astro"}
  @forges %{"github" => {ForgeMock, :client}}

  defp start(opts) do
    opts = Keyword.merge([name: nil, tick_ms: 60_000, forges: @forges, async: false], opts)
    pid = start_supervised!({Manager, opts})
    Ecto.Adapters.SQL.Sandbox.allow(KuiperCoordinator.Repo, self(), pid)
    pid
  end

  test "fixed capacity drains switched-off agents before it reconciles (F16)" do
    test = self()

    {:ok, _} =
      KuiperCoordinator.Runners.add_runner(%{
        runner_name: "mac1-00000001",
        agent_id: "a1",
        vm_name: "mac1-00000001",
        runner_scope: @scope,
        forge: "github"
      })

    stub(AgentDispatchMock, :unavailable_agents, fn -> ["a1"] end)

    expect(ForgeMock, :remove_idle_runner, fn :client, @scope, "mac1-00000001" ->
      {:ok, :removed}
    end)

    expect(AgentDispatchMock, :dispatch_destroy, fn "a1", "mac1-00000001" ->
      send(test, :destroyed)
      :ok
    end)

    expect(AgentDispatchMock, :pool_definitions, fn ->
      send(test, :reconciled)
      []
    end)

    pid = start(mode: :fixed_capacity, runner_scope: @scope)
    send(pid, :tick)
    # one sender, so messages arrive in order: the drain ran first
    assert_receive first
    assert first == :destroyed
    assert_receive :reconciled
    assert %{draining: %{"mac1-00000001" => _}} = :sys.get_state(pid)
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

  describe "forges in different modes, side by side" do
    @mixed %{
      "pools" => {ForgeMock, :pools},
      "hooked" => {ForgeMock, :hooked},
      "polled" => {ForgeMock, :polled}
    }
    @modes %{"pools" => :fixed_capacity, "hooked" => :webhook, "polled" => :polling}

    test "the fixed-capacity forge fills the pools; only the others are scanned" do
      test = self()

      stub(AgentDispatchMock, :pool_definitions, fn ->
        send(test, :reconciled)
        []
      end)

      # :pools is never listed: Mox fails the test if it is
      expect(ForgeMock, :list_queued_jobs, 2, fn client when client in [:hooked, :polled] ->
        send(test, {:listed, client})
        {:ok, []}
      end)

      pid = start(forges: @mixed, modes: @modes, runner_scope: @scope)
      assert_receive :reconciled
      assert_receive {:listed, :hooked}
      assert_receive {:listed, :polled}

      state = :sys.get_state(pid)
      assert state.modes == @modes
      assert %{forge_name: "pools", forge: {ForgeMock, :pools}, scope: @scope} = state.ctx

      assert %{
               "hooked" => %{kind: :webhook, every_ms: 300_000, last: hooked},
               "polled" => %{kind: :polling, every_ms: 120_000, last: polled}
             } = state.scans

      assert is_integer(hooked) and is_integer(polled)
      refute is_map_key(state.scans, "pools")
      assert state.idle
    end

    test "a forge is scanned again only once its own interval has passed" do
      test = self()
      stub(AgentDispatchMock, :pool_definitions, fn -> [] end)

      stub(ForgeMock, :list_queued_jobs, fn client ->
        send(test, {:listed, client})
        {:ok, []}
      end)

      pid = start(forges: @mixed, modes: @modes, runner_scope: @scope)
      assert_receive {:listed, :hooked}
      assert_receive {:listed, :polled}

      # the poll is due at once, the webhook forge's scan isn't
      :sys.replace_state(pid, &put_in(&1.scans["polled"].every_ms, 0))
      send(pid, :tick)
      assert_receive {:listed, :polled}
      _ = :sys.get_state(pid)
      refute_received {:listed, :hooked}
    end

    test "with no fixed-capacity forge the pools are left alone" do
      stub(AgentDispatchMock, :select_agent, fn _, _ -> {:error, :no_available_agent} end)
      stub(ForgeMock, :list_queued_jobs, fn _ -> {:ok, []} end)

      # no pool_definitions expectation: Mox fails the test if the pools are reconciled
      pid = start(forges: @mixed, modes: %{@modes | "pools" => :polling})
      send(pid, :tick)
      assert %{ctx: %{forge: nil, forge_name: nil, scope: nil}} = :sys.get_state(pid)
    end
  end

  test "fixed capacity with no runner scope creates nothing" do
    pid = start(mode: :fixed_capacity, runner_scope: nil)
    send(pid, :tick)
    # no pool_definitions expectation: Mox fails the test if it's called
    assert %{modes: %{"github" => :fixed_capacity}} = :sys.get_state(pid)
  end

  test "webhook mode processes pending jobs on a new job, a reprocess and a tick (P6, W7)" do
    {:ok, _} =
      Jobs.add_pending(%{job_key: "github:1", forge: "github", runner_scope: @scope})

    test = self()
    stub(ForgeMock, :list_queued_jobs, fn :client -> {:ok, []} end)

    # startup pass, then one each for the new job, the reprocess and the tick
    expect(AgentDispatchMock, :select_agent, 4, fn _, _ ->
      send(test, :processed)
      {:error, :no_available_agent}
    end)

    pid = start(mode: :webhook, scan_ms: 60_000)
    assert_receive :processed
    Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "fleet", {:new_job, "github:1"})
    assert_receive :processed
    Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "fleet", :reprocess)
    assert_receive :processed
    send(pid, :tick)
    assert_receive :processed
    # a started job is metrics only in Rust: no pass
    Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "fleet", {:job_started, "github:1", nil})
    :sys.get_state(pid)
    refute_received :processed
  end

  test "a completed job leaves the pending queue and the freed capacity gets a pass (Rust JobCompleted)" do
    for key <- ["github:1", "github:2"],
        do: {:ok, _} = Jobs.add_pending(%{job_key: key, forge: "github", runner_scope: @scope})

    test = self()
    stub(ForgeMock, :list_queued_jobs, fn :client -> {:ok, []} end)

    stub(AgentDispatchMock, :select_agent, fn _, _ ->
      send(test, :processed)
      nil
    end)

    pid = start(mode: :webhook, scan_ms: 60_000)
    assert_receive :processed
    :sys.get_state(pid)
    flush_processed()

    Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "fleet", {:job_completed, "github:1"})
    assert_receive :processed
    :sys.get_state(pid)
    assert Jobs.get_pending("github:1") == nil
    assert Jobs.get_pending("github:2")
  end

  defp flush_processed do
    receive do
      :processed -> flush_processed()
    after
      0 -> :ok
    end
  end

  test "a crashing pass doesn't take the manager down" do
    expect(AgentDispatchMock, :pool_definitions, fn -> raise "boom" end)
    pid = start(mode: :fixed_capacity, runner_scope: @scope)
    send(pid, :tick)
    assert %{modes: %{"github" => :fixed_capacity}} = :sys.get_state(pid)
    assert Process.alive?(pid)
  end

  test "an unknown mode is refused at start" do
    assert {:error, {:unknown_mode, :polling_v2}} =
             Manager.init(mode: :polling_v2, forges: @forges, tick_ms: 1)
  end

  describe "forge scan (Rust scan_forges in run_on_demand_loop)" do
    test "at startup: jobs left from a previous run first, then the scan, then recovered jobs" do
      {:ok, _} =
        Jobs.add_pending(%{
          job_key: "github:1",
          forge: "github",
          runner_scope: @scope,
          repository: "astro/app"
        })

      test = self()

      expect(AgentDispatchMock, :select_agent, 2, fn labels, _ ->
        send(test, {:pass, labels})
        {:error, :no_available_agent}
      end)

      expect(ForgeMock, :job_status, fn :client, "astro/app", "1" ->
        send(test, :verified)
        {:ok, "completed"}
      end)

      expect(ForgeMock, :list_queued_jobs, fn :client ->
        send(test, :scanned)

        {:ok, [%QueuedJob{id: "2", labels: ["self-hosted", "macos"], runner_scope: @scope}]}
      end)

      start(mode: :webhook, webhook: [])
      assert_receive {:pass, []}
      assert_receive :verified
      assert_receive :scanned
      assert_receive {:pass, ["self-hosted", "macos"]}
      assert [%{job_key: "github:2"}] = Jobs.list_pending()
    end

    test "a tick scans again once the scan interval has passed" do
      test = self()
      stub(AgentDispatchMock, :select_agent, fn _, _ -> {:error, :no_available_agent} end)

      expect(ForgeMock, :list_queued_jobs, 3, fn :client ->
        send(test, :scanned)
        {:ok, []}
      end)

      pid = start(mode: :webhook, scan_ms: 0)
      assert_receive :scanned
      send(pid, :tick)
      assert_receive :scanned
      send(pid, :tick)
      assert_receive :scanned
    end

    test "a tick before the scan interval doesn't scan" do
      test = self()
      stub(AgentDispatchMock, :select_agent, fn _, _ -> {:error, :no_available_agent} end)

      expect(ForgeMock, :list_queued_jobs, 1, fn :client ->
        send(test, :scanned)
        {:ok, []}
      end)

      pid = start(mode: :webhook, scan_ms: 60_000)
      assert_receive :scanned
      send(pid, :tick)
      :sys.get_state(pid)
      refute_received :scanned
    end

    test "a scan that crashes is logged and the manager carries on" do
      expect(ForgeMock, :list_queued_jobs, fn :client -> raise "boom" end)
      pid = start(mode: :webhook, scan_ms: 60_000)

      assert %{modes: %{"github" => :webhook}, scans: %{"github" => %{last: at}}} =
               :sys.get_state(pid)

      assert is_integer(at)
      assert Process.alive?(pid)
    end

    test "fixed capacity never scans" do
      stub(AgentDispatchMock, :pool_definitions, fn -> [] end)
      pid = start(mode: :fixed_capacity, runner_scope: @scope, scan_ms: 0)
      send(pid, :tick)
      # no list_queued_jobs expectation: Mox fails the test if it's called
      assert :sys.get_state(pid).scans == %{}
    end
  end

  describe "polling mode (Rust ProvisioningMode::Polling)" do
    test "polls at startup and processes what it found, then polls only pending jobs it missed" do
      test = self()

      expect(AgentDispatchMock, :select_agent, fn labels, _ ->
        send(test, {:pass, labels})
        {:error, :no_available_agent}
      end)

      expect(ForgeMock, :list_queued_jobs, fn :client ->
        send(test, :polled)

        {:ok,
         [
           %QueuedJob{
             id: "5",
             labels: ["self-hosted"],
             runner_scope: @scope,
             repository: "astro/app"
           }
         ]}
      end)

      # the job it just found is listed, so it's never checked with job_status
      start(mode: :polling, scan_ms: 60_000)
      assert_receive :polled
      assert_receive {:pass, ["self-hosted"]}
      assert [%{job_key: "github:5"}] = Jobs.list_pending()
    end

    test "the poll interval sets the scan, and the tick is never slower than it" do
      {:ok, state, _} = Manager.init(mode: :polling, forges: @forges, poll_interval_s: 15)
      assert %{scans: %{"github" => %{every_ms: 15_000}}, tick_ms: 15_000} = state
      {:ok, state, _} = Manager.init(mode: :polling, forges: @forges, poll_interval_s: 1)
      assert %{scans: %{"github" => %{every_ms: 10_000}}, tick_ms: 10_000} = state
      {:ok, state, _} = Manager.init(mode: :polling, forges: @forges)
      assert %{scans: %{"github" => %{every_ms: 120_000}}, tick_ms: 30_000} = state
    end

    test "a tick polls again once the interval has passed, and a new job gets a pass" do
      test = self()
      stub(AgentDispatchMock, :select_agent, fn _, _ -> {:error, :no_available_agent} end)

      expect(ForgeMock, :list_queued_jobs, 2, fn :client ->
        send(test, :polled)
        {:ok, []}
      end)

      pid = start(mode: :polling, scan_ms: 0)
      assert_receive :polled
      send(pid, :tick)
      assert_receive :polled
      Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "fleet", :reprocess)
      assert %{modes: %{"github" => :polling}} = :sys.get_state(pid)
    end
  end
end
