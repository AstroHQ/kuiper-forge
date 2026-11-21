defmodule KuiperCoordinator.AgentDispatch.RegistryTest do
  # The registry-backed AgentDispatch (D13, D17-D19): capacity lives in the agent registry,
  # reservations are atomic there, and commands go over the agent's stream.
  use ExUnit.Case, async: false
  @moduletag :capture_log

  alias Cirunner.Agent.{
    AgentStatus,
    CommandAck,
    CommandResult,
    CreateRunnerCommand,
    LabelSet,
    VmInfo
  }

  alias KuiperCoordinator.AgentDispatch.Registry, as: Dispatch
  alias KuiperCoordinator.AgentRegistry
  alias KuiperCoordinator.Fleet.PoolDefinition

  defp status(max, labels, opts \\ []) do
    %AgentStatus{
      hostname: Keyword.get(opts, :hostname, "host"),
      max_vms: max,
      active_vms: Keyword.get(opts, :active, 0),
      labels: labels,
      label_sets: Keyword.get(opts, :sets, []),
      runner_kinds: Keyword.get(opts, :kinds, []),
      vms: Enum.map(Keyword.get(opts, :vms, []), &%VmInfo{name: &1, vm_id: &1})
    }
  end

  # A stand-in AgentStream process: registers with `status`, answers commands with `reply`
  # (a function of the command id), and forwards pushes to the test.
  defp fake_agent(
         id,
         status,
         reply \\ fn cid -> {:ack, %CommandAck{command_id: cid, accepted: true}} end
       ) do
    parent = self()

    start_supervised!(
      {Task,
       fn ->
         :ok =
           AgentRegistry.register(id, %{
             hostname: status.hostname,
             agent_type: :tart,
             status: status
           })

         send(parent, {:up, id})
         fake_loop(parent, reply)
       end},
      id: id
    )

    assert_receive {:up, ^id}
    id
  end

  defp fake_loop(parent, reply) do
    receive do
      {:send_command, caller, ref, cid, payload} ->
        send(parent, {:command, cid, payload})

        case reply.(cid) do
          :silent -> :ok
          answer -> send(caller, {ref, answer})
        end

        fake_loop(parent, reply)

      {:push, payload} ->
        send(parent, {:pushed, payload})
        fake_loop(parent, reply)

      {:run, fun, from, ref} ->
        fun.()
        send(from, {:ran, ref})
        fake_loop(parent, reply)

      {:disconnect, _} ->
        :ok
    end
  end

  defp uid(prefix), do: "#{prefix}_#{System.unique_integer([:positive])}"

  test "reserve_slot picks the least-loaded matching agent and counts the reservation" do
    a = fake_agent(uid("a"), status(2, ["macos"]))
    b = fake_agent(uid("b"), status(2, ["macos"], active: 1))
    _linux = fake_agent(uid("l"), status(4, ["linux"]))

    assert {:ok, ^a} = Dispatch.reserve_slot(["macos"], "r1", [])
    # a: 1 free, 1 reserved; b: 1 free, 1 active. Tie on free and load, so the lower id wins
    assert {:ok, ^a} = Dispatch.reserve_slot(["macos"], "r2", [])
    assert {:ok, ^b} = Dispatch.reserve_slot(["macos"], "r3", [])
    assert {:error, :no_available_agent} = Dispatch.reserve_slot(["macos"], "r4", [])

    :ok = Dispatch.release_slot(a, "r1")
    assert {:ok, ^a} = Dispatch.reserve_slot(["macos"], "r5", [])
  end

  test "concurrent reservations never overbook an agent" do
    a = fake_agent(uid("a"), status(3, ["macos"]))

    results =
      1..20
      |> Task.async_stream(&Dispatch.reserve_slot(["macos"], "c#{&1}", []), max_concurrency: 20)
      |> Enum.map(fn {:ok, r} -> r end)

    assert Enum.count(results, &(&1 == {:ok, a})) == 3
    assert Enum.count(results, &(&1 == {:error, :no_available_agent})) == 17
  end

  test "select_agent avoids failed agents unless nobody else fits; agent_id reserves on that agent" do
    a = fake_agent(uid("a"), status(1, ["macos"]))
    b = fake_agent(uid("b"), status(1, ["macos"]))

    {:ok, first} = Dispatch.select_agent(["macos"], [])
    other = if first == a, do: b, else: a
    assert {:ok, ^other} = Dispatch.select_agent(["macos"], avoid: [first])
    assert {:ok, ^first} = Dispatch.select_agent(["MacOS"], avoid: [])

    assert {:ok, ^b} = Dispatch.reserve_slot(["macos"], "x", agent_id: b)
    assert {:error, :no_available_agent} = Dispatch.reserve_slot(["macos"], "y", agent_id: b)
    assert {:error, :no_available_agent} = Dispatch.reserve_slot(["linux"], "z", agent_id: a)
    assert {:error, :no_available_agent} = Dispatch.reserve_slot(["macos"], "w", agent_id: "gone")
    assert Dispatch.hostname(b) == "host"
    assert Dispatch.hostname("gone") == nil
  end

  test "Forgejo runners only go to agents listing the forgejo runner kind (R5)" do
    tart = fake_agent(uid("a"), status(4, ["macos"]))
    fj = fake_agent(uid("b"), status(1, ["macos"], kinds: ["Forgejo"]))

    # GitHub runs everywhere, so the least-loaded agent wins
    assert {:ok, ^tart} = Dispatch.select_agent(["macos"], kind: "github")
    assert {:ok, ^tart} = Dispatch.select_agent(["macos"], [])
    # forgejo: only the agent that lists it (case-insensitive), even with less room
    assert {:ok, ^fj} = Dispatch.select_agent(["macos"], kind: "forgejo")

    assert {:error, :no_available_agent} =
             Dispatch.reserve_slot(["macos"], "f0", agent_id: tart, kind: "forgejo")

    assert {:ok, ^fj} = Dispatch.reserve_slot(["macos"], "f1", kind: "forgejo")
    assert {:error, :no_available_agent} = Dispatch.reserve_slot(["macos"], "f2", kind: "forgejo")
    assert {:error, :no_available_agent} = Dispatch.select_agent(["macos"], kind: "forgejo")
  end

  test "a reservation settles once the VM shows up in a status, without double counting" do
    id = uid("a")
    st = status(2, ["macos"])
    a = fake_agent(id, st)

    assert {:ok, ^a} = Dispatch.reserve_slot(["macos"], "r1", [])
    assert {:ok, ^a} = Dispatch.reserve_slot(["macos"], "r2", [])
    assert {:error, :no_available_agent} = Dispatch.reserve_slot(["macos"], "r3", [])

    # r1's VM is now running: it counts once (as active), not as active + reserved
    send_status(a, %{st | active_vms: 1, vms: [%VmInfo{name: "r1"}]})
    assert {:error, :no_available_agent} = Dispatch.reserve_slot(["macos"], "r3", [])
    assert Map.keys(AgentRegistry.lookup(a).capacity.reservations) == ["r2"]

    # r1 finished; release of an already-settled name is a no-op
    send_status(a, %{st | active_vms: 0, vms: []})
    :ok = Dispatch.release_slot(a, "r1")
    assert {:ok, ^a} = Dispatch.reserve_slot(["macos"], "r3", [])
  end

  test "dispatch_create: accepted ack, rejected ack, legacy result, timeout, not connected" do
    a = fake_agent(uid("ok"), status(1, ["macos"]))
    cmd = %CreateRunnerCommand{vm_name: "run-1", labels: ["macos"]}
    assert :ok = Dispatch.dispatch_create(a, cmd)
    assert_receive {:command, cid, {:create_runner, %CreateRunnerCommand{command_id: cid}}}
    assert cid != ""

    r =
      fake_agent(uid("rej"), status(1, ["macos"]), fn cid ->
        {:ack, %CommandAck{command_id: cid, accepted: false, error: "at capacity"}}
      end)

    assert {:error, {:rejected, "at capacity"}} = Dispatch.dispatch_create(r, cmd)

    l =
      fake_agent(uid("legacy"), status(1, ["macos"]), fn cid ->
        {:result, %CommandResult{command_id: cid, success: true}}
      end)

    assert {:ok, %CommandResult{success: true}} = Dispatch.dispatch_create(l, cmd)

    s = fake_agent(uid("silent"), status(1, ["macos"]), fn _ -> :silent end)
    Application.put_env(:kuiper_coordinator, :dispatch_ack_timeout_ms, 100)
    on_exit(fn -> Application.delete_env(:kuiper_coordinator, :dispatch_ack_timeout_ms) end)
    assert {:error, :timeout} = Dispatch.dispatch_create(s, cmd)

    assert {:error, :not_connected} = Dispatch.dispatch_create("nobody", cmd)
  end

  test "dispatch_destroy pushes DestroyRunner with the VM name as vm_id (F8)" do
    a = fake_agent(uid("a"), status(1, ["macos"]))
    assert :ok = Dispatch.dispatch_destroy(a, "run-7")
    assert_receive {:pushed, {:destroy_runner, %{vm_id: "run-7", command_id: cid}}}
    assert cid != ""
    assert {:error, :not_connected} = Dispatch.dispatch_destroy("nobody", "run-7")
  end

  test "pool definitions, capacity and pool agent selection come from connected agents" do
    l1 = fake_agent(uid("legacy1"), status(2, ["Linux", "x64"]))
    l2 = fake_agent(uid("legacy2"), status(1, ["x64", "linux"]))

    e =
      fake_agent(
        uid("explicit"),
        status(4, ["macos"],
          sets: [%LabelSet{labels: ["macos", "arm64"], pool_size: 2, id: "s1"}]
        )
      )

    pools = Dispatch.pool_definitions()
    legacy = Enum.find(pools, &(&1.agent_id == nil and &1.labels == ["linux", "x64"]))
    assert legacy.target_count == 3
    assert Enum.sort(legacy.members) == Enum.sort([l1, l2])
    explicit = Enum.find(pools, &(&1.agent_id == e))
    assert %PoolDefinition{label_set_id: "s1", target_count: 2} = explicit

    assert Dispatch.pool_capacity(legacy) == 3
    assert {:ok, ^l1} = Dispatch.select_pool_agent(legacy)
    assert {:ok, ^l1} = Dispatch.reserve_slot(["linux", "x64"], "p1", agent_id: l1)
    assert {:ok, ^l1} = Dispatch.reserve_slot(["linux", "x64"], "p2", agent_id: l1)
    assert Dispatch.pool_capacity(legacy) == 1
    assert {:ok, ^l2} = Dispatch.select_pool_agent(legacy)

    assert Dispatch.pool_capacity(explicit) == 4
    assert {:ok, ^e} = Dispatch.select_pool_agent(explicit)
  end

  test "an agent switched off on the dashboard gets no new runners until it's back (#40)" do
    a = fake_agent(uid("a"), status(2, ["macos"]))
    b = fake_agent(uid("b"), status(1, ["macos"]))
    assert Dispatch.available_capacity(["macos"]) == 3

    set_available(a, false)
    assert Dispatch.available_capacity(["macos"]) == 1
    assert {:ok, ^b} = Dispatch.reserve_slot(["macos"], "r1", [])
    assert {:error, :no_available_agent} = Dispatch.reserve_slot(["macos"], "r2", [])
    assert {:error, :no_available_agent} = Dispatch.reserve_slot(["macos"], "r3", agent_id: a)
    assert Enum.all?(Dispatch.pool_definitions(), &(a not in &1.members))

    # A new status keeps it switched off
    send_status(a, status(2, ["macos"]))
    assert Dispatch.available_capacity(["macos"]) == 0

    set_available(a, true)
    assert Dispatch.available_capacity(["macos"]) == 2
    assert {:ok, ^a} = Dispatch.reserve_slot(["macos"], "r4", [])
  end

  test "an agent switched off while offline stays off when it connects" do
    id = uid("late")
    set_available(id, false)
    ^id = fake_agent(id, status(2, ["macos"]))
    assert {:error, :no_available_agent} = Dispatch.reserve_slot(["macos"], "r1", agent_id: id)
    set_available(id, true)
    assert {:ok, ^id} = Dispatch.reserve_slot(["macos"], "r1", agent_id: id)
  end

  # touch only counts from the stream process that owns the entry, so run it there
  # What Agents.set_unavailable/2 sends (#40); the registry handles it before the next read
  defp set_available(agent_id, available?) do
    AgentRegistry.broadcast({:agent_availability, agent_id, available?})
    _ = :sys.get_state(AgentRegistry)
  end

  defp send_status(agent_id, st) do
    owner = AgentRegistry.lookup(agent_id).pid
    ref = make_ref()
    send(owner, {:run, fn -> AgentRegistry.touch(agent_id, st) end, self(), ref})
    assert_receive {:ran, ^ref}
    _ = :sys.get_state(AgentRegistry)
  end
end
