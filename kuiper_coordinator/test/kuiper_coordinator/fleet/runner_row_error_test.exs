defmodule KuiperCoordinator.Fleet.RunnerRowErrorTest do
  # F8: the runner row can't be written (DB error) after a slot was reserved.
  use KuiperCoordinator.DataCase, async: false
  import Mox
  import ExUnit.CaptureLog

  alias KuiperCoordinator.{AgentDispatchMock, ForgeMock, Jobs, Repo}
  alias KuiperCoordinator.Fleet.{FixedCapacity, OnDemand, PoolDefinition}

  setup :verify_on_exit!

  @scope {:org, "astro"}

  defp break_runners_table, do: Repo.query!("ALTER TABLE runners RENAME TO runners_gone")

  test "fixed capacity: a failed row write gives the reserved slot back" do
    pool = %PoolDefinition{labels: ["macos", "self-hosted"], target_count: 1, members: ["a1"]}
    ctx = %{scope: @scope, forge_name: "github", forge: {ForgeMock, :client}, async: false}
    stub(ForgeMock, :scope_url, fn _, _ -> "https://github.com/astro" end)
    expect(AgentDispatchMock, :pool_capacity, fn _ -> 1 end)
    expect(AgentDispatchMock, :select_pool_agent, fn _ -> {:ok, "a1"} end)
    stub(AgentDispatchMock, :hostname, fn _ -> "mac1" end)
    expect(AgentDispatchMock, :reserve_slot, fn _, _, _ -> {:ok, "a1"} end)

    expect(ForgeMock, :registration_token, fn _, _ ->
      # the pool was counted; the table goes away before the row is written
      break_runners_table()
      {:ok, "REG"}
    end)

    # no row, no CreateRunner (F3)
    expect(AgentDispatchMock, :dispatch_create, 0, fn _, _ -> :ok end)
    # the slot must come back, or the agent loses that capacity until it reconnects
    expect(AgentDispatchMock, :release_slot, fn "a1", _ -> :ok end)

    log = capture_log(fn -> FixedCapacity.reconcile_pool(pool, ctx) end)
    assert log =~ "could not record runner"
  end

  test "on demand: a failed row write releases the slot and removes the JIT runner" do
    {:ok, :inserted} =
      Jobs.add_pending(%{
        job_key: "github:42",
        forge: "github",
        job_labels: ["macos"],
        agent_labels: ["macos"],
        runner_scope: @scope,
        repository: "astro/app"
      })

    ctx = %{forges: %{"github" => {ForgeMock, :client}}, async: false}
    stub(ForgeMock, :scope_url, fn _, _ -> "https://github.com/astro" end)
    expect(AgentDispatchMock, :select_agent, fn _, _ -> {:ok, "a1"} end)
    stub(AgentDispatchMock, :hostname, fn _ -> "mac1" end)
    expect(AgentDispatchMock, :reserve_slot, fn _, _, _ -> {:ok, "a1"} end)
    expect(ForgeMock, :job_status, fn _, _, _ -> {:ok, "queued"} end)

    expect(ForgeMock, :create_job_runner, fn _, _, _, _, _, _ ->
      # the runners table goes away once the job list was read
      break_runners_table()
      {:ok, {:github_jit, "JIT"}}
    end)

    # no row, no CreateRunner (F3)
    expect(AgentDispatchMock, :dispatch_create, 0, fn _, _ -> :ok end)
    expect(AgentDispatchMock, :release_slot, fn "a1", _ -> :ok end)
    expect(ForgeMock, :remove_runner, fn _, _, _ -> :ok end)

    log = capture_log(fn -> assert %{created: 0} = OnDemand.process_pending(ctx) end)
    assert log =~ "could not record runner"
  end
end
