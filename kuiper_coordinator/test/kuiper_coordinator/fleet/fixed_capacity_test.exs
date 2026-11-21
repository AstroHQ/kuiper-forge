defmodule KuiperCoordinator.Fleet.FixedCapacityTest do
  use KuiperCoordinator.DataCase, async: true
  import Mox

  alias KuiperCoordinator.{AgentDispatchMock, ForgeMock, Runners}
  alias KuiperCoordinator.Fleet.{FixedCapacity, PoolDefinition}
  alias Cirunner.Agent.CreateRunnerCommand

  setup :verify_on_exit!

  @scope {:org, "astro"}
  @ctx %{scope: @scope, forge_name: "github", forge: {ForgeMock, :client}, async: false}

  defp legacy(target, members \\ ["a1"]),
    do: %PoolDefinition{labels: ["macos", "self-hosted"], target_count: target, members: members}

  defp explicit(target),
    do: %PoolDefinition{
      labels: ["linux", "self-hosted"],
      target_count: target,
      agent_id: "a1",
      label_set_id: "ubuntu"
    }

  defp row(name, agent, pool \\ nil) do
    {:ok, _} =
      Runners.add_runner(%{
        runner_name: name,
        agent_id: agent,
        vm_name: name,
        runner_scope: @scope,
        forge: "github",
        pool: pool
      })
  end

  defp stub_forge do
    stub(ForgeMock, :scope_url, fn :client, @scope -> "https://github.com/astro" end)
  end

  describe "pool counts come from runner rows (P3)" do
    test "a legacy pool counts only its member agents' runners" do
      row("mac1-00000001", "a1")
      # a2 has superset labels, so it isn't a member of this pool
      row("mac2-00000002", "a2")
      assert FixedCapacity.current_count(legacy(2)) == 1
    end

    test "an explicit pool counts that agent's runners tagged with the pool key" do
      pool = explicit(2)
      row("lin1-00000001", "a1", PoolDefinition.key(pool))
      row("lin1-00000002", "a1", nil)
      row("lin2-00000003", "a2", PoolDefinition.key(pool))
      assert FixedCapacity.current_count(pool) == 1
      assert PoolDefinition.key(pool) == "linux,self-hosted@ubuntu"
    end
  end

  test "a full pool creates nothing" do
    row("mac1-00000001", "a1")
    row("mac1-00000002", "a1")
    assert FixedCapacity.reconcile_pool(legacy(2), @ctx) == {:ok, 0}
  end

  test "creates min(target - current, capacity), writing the row before CreateRunner (P4, F3)" do
    stub_forge()
    pool = legacy(3)
    expect(AgentDispatchMock, :pool_capacity, fn ^pool -> 2 end)
    expect(AgentDispatchMock, :select_pool_agent, 2, fn ^pool -> {:ok, "a1"} end)
    expect(AgentDispatchMock, :hostname, 2, fn "a1" -> "mac1.local" end)

    expect(AgentDispatchMock, :reserve_slot, 2, fn ["macos", "self-hosted"], name, opts ->
      assert opts[:agent_id] == "a1"
      assert opts[:set_id] == ""
      assert name =~ ~r/^mac1-[0-9a-f]{8}$/
      {:ok, "a1"}
    end)

    expect(ForgeMock, :registration_token, 2, fn :client, @scope -> {:ok, "REG"} end)

    expect(AgentDispatchMock, :dispatch_create, 2, fn "a1", %CreateRunnerCommand{} = cmd ->
      assert %{registration_token: "REG", jit_config: "", label_set_id: ""} = cmd
      assert cmd.runner_scope_url == "https://github.com/astro"
      assert cmd.labels == ["macos", "self-hosted"]
      row = Runners.get_runner(cmd.vm_name)
      assert row, "row must exist before CreateRunner is sent"
      assert %{agent_id: "a1", vm_name: vm, forge: "github", pool: nil, job_key: nil} = row
      assert vm == cmd.vm_name
      :ok
    end)

    assert FixedCapacity.reconcile_pool(pool, @ctx) == {:ok, 2}
    assert length(Runners.list_runners()) == 2
  end

  test "an explicit pool uses its own agent and tags runners with the pool key" do
    stub_forge()
    pool = explicit(1)
    expect(AgentDispatchMock, :pool_capacity, fn ^pool -> 5 end)
    expect(AgentDispatchMock, :hostname, fn "a1" -> "lin1" end)

    expect(AgentDispatchMock, :reserve_slot, fn _, _, opts ->
      assert opts[:agent_id] == "a1" and opts[:set_id] == "ubuntu"
      {:ok, "a1"}
    end)

    expect(ForgeMock, :registration_token, fn _, _ -> {:ok, "REG"} end)

    expect(AgentDispatchMock, :dispatch_create, fn "a1", cmd ->
      assert cmd.label_set_id == "ubuntu"
      :ok
    end)

    assert FixedCapacity.reconcile_pool(pool, @ctx) == {:ok, 1}
    assert [%{pool: "linux,self-hosted@ubuntu"}] = Runners.list_runners()
  end

  test "no capacity or no agent creates nothing" do
    pool = legacy(2)
    expect(AgentDispatchMock, :pool_capacity, fn ^pool -> 0 end)
    assert FixedCapacity.reconcile_pool(pool, @ctx) == {:ok, 0}

    expect(AgentDispatchMock, :pool_capacity, fn ^pool -> 2 end)
    expect(AgentDispatchMock, :select_pool_agent, fn ^pool -> :none end)
    assert FixedCapacity.reconcile_pool(pool, @ctx) == {:ok, 0}
  end

  test "a reserve failure moves on to the next runner" do
    stub_forge()
    pool = legacy(2)
    expect(AgentDispatchMock, :pool_capacity, fn _ -> 2 end)
    stub(AgentDispatchMock, :select_pool_agent, fn _ -> {:ok, "a1"} end)
    stub(AgentDispatchMock, :hostname, fn _ -> "mac1" end)

    expect(AgentDispatchMock, :reserve_slot, 2, fn _, _, _ -> {:error, :no_available_agent} end)

    assert FixedCapacity.reconcile_pool(pool, @ctx) == {:ok, 0}
    assert Runners.list_runners() == []
  end

  test "a registration token failure releases the slot, writes no row and stops (P4)" do
    pool = legacy(3)
    expect(AgentDispatchMock, :pool_capacity, fn _ -> 3 end)
    expect(AgentDispatchMock, :select_pool_agent, fn _ -> {:ok, "a1"} end)
    expect(AgentDispatchMock, :hostname, fn _ -> "mac1" end)
    expect(AgentDispatchMock, :reserve_slot, fn _, _, _ -> {:ok, "a1"} end)
    expect(ForgeMock, :registration_token, fn _, _ -> {:error, :forbidden} end)
    test_pid = self()

    expect(AgentDispatchMock, :release_slot, fn "a1", name ->
      send(test_pid, {:released, name})
      :ok
    end)

    assert FixedCapacity.reconcile_pool(pool, @ctx) == {:ok, 0}
    assert_received {:released, "mac1-" <> _}
    assert Runners.list_runners() == []
  end

  for {label, reply} <- [
        rejected: {:error, {:rejected, "at capacity"}},
        timeout: {:error, :timeout},
        disconnected: {:error, :disconnected}
      ] do
    test "a #{label} CreateRunner releases the slot, removes the forge runner and the row (P5)" do
      stub_forge()
      pool = legacy(1)
      expect(AgentDispatchMock, :pool_capacity, fn _ -> 1 end)
      expect(AgentDispatchMock, :select_pool_agent, fn _ -> {:ok, "a1"} end)
      expect(AgentDispatchMock, :hostname, fn _ -> "mac1" end)
      expect(AgentDispatchMock, :reserve_slot, fn _, _, _ -> {:ok, "a1"} end)
      expect(ForgeMock, :registration_token, fn _, _ -> {:ok, "REG"} end)
      expect(AgentDispatchMock, :dispatch_create, fn _, _ -> unquote(Macro.escape(reply)) end)
      test_pid = self()

      expect(AgentDispatchMock, :release_slot, fn "a1", name ->
        send(test_pid, {:released, name})
        :ok
      end)

      expect(ForgeMock, :remove_runner, fn :client, @scope, name ->
        send(test_pid, {:forge_removed, name})
        :ok
      end)

      assert FixedCapacity.reconcile_pool(pool, @ctx) == {:ok, 1}
      assert_received {:released, name}
      assert_received {:forge_removed, ^name}
      assert Runners.list_runners() == []
    end
  end

  test "a legacy agent's Result removes the finished runner and its forge registration" do
    stub_forge()
    pool = legacy(1)
    expect(AgentDispatchMock, :pool_capacity, fn _ -> 1 end)
    expect(AgentDispatchMock, :select_pool_agent, fn _ -> {:ok, "a1"} end)
    expect(AgentDispatchMock, :hostname, fn _ -> "mac1" end)
    expect(AgentDispatchMock, :reserve_slot, fn _, _, _ -> {:ok, "a1"} end)
    expect(ForgeMock, :registration_token, fn _, _ -> {:ok, "REG"} end)

    expect(AgentDispatchMock, :dispatch_create, fn _, _ ->
      {:ok, %Cirunner.Agent.CommandResult{success: true}}
    end)

    expect(AgentDispatchMock, :release_slot, fn "a1", _ -> :ok end)
    expect(ForgeMock, :remove_runner, fn _, _, _ -> :ok end)

    assert FixedCapacity.reconcile_pool(pool, @ctx) == {:ok, 1}
    assert Runners.list_runners() == []
  end

  test "reconcile without a runner scope creates nothing (P2)" do
    assert FixedCapacity.reconcile(%{@ctx | scope: nil}) == {:error, :no_runner_scope}
  end

  test "reconcile tops up every pool the agents define" do
    row("mac1-00000001", "a1")
    row("mac1-00000002", "a1")
    expect(AgentDispatchMock, :pool_definitions, fn -> [legacy(2), legacy(1, [])] end)
    expect(AgentDispatchMock, :pool_capacity, fn _ -> 0 end)
    assert FixedCapacity.reconcile(@ctx) == {:ok, 0}
  end
end
