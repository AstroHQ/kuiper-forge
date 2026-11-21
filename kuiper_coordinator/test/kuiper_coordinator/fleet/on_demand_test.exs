defmodule KuiperCoordinator.Fleet.OnDemandTest do
  use KuiperCoordinator.DataCase, async: false
  import Mox

  alias KuiperCoordinator.{AgentDispatchMock, ForgeMock, Jobs, Runners}
  alias KuiperCoordinator.Fleet.OnDemand
  alias Cirunner.Agent.{CommandResult, CreateRunnerCommand}

  setup :verify_on_exit!

  @scope {:org, "astro"}
  @labels ["macos", "self-hosted"]
  @ctx %{forges: %{"github" => {ForgeMock, :client}}, async: false}

  setup do
    Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, "fleet")
    :ok
  end

  defp job(id, attrs \\ %{}) do
    {:ok, :inserted} =
      Jobs.add_pending(
        Map.merge(
          %{
            job_key: "github:#{id}",
            forge: "github",
            job_labels: @labels,
            agent_labels: @labels,
            runner_scope: @scope,
            repository: "astro/app",
            job_name: "build"
          },
          attrs
        )
      )

    "github:#{id}"
  end

  defp retry(key), do: Jobs.get_pending(key).retry_count

  # Agent a1 is picked, reserved, the job is still queued and the forge hands out a JIT config.
  defp happy_path(key_id \\ "42", status \\ {:ok, "queued"}) do
    expect(AgentDispatchMock, :select_agent, fn @labels, opts ->
      assert opts[:avoid] == []
      {:ok, "a1"}
    end)

    expect(AgentDispatchMock, :hostname, fn "a1" -> "mac1.local" end)

    expect(AgentDispatchMock, :reserve_slot, fn @labels, name, opts ->
      assert name =~ ~r/^mac1-[0-9a-f]{8}$/
      assert opts[:agent_id] == "a1"
      {:ok, "a1"}
    end)

    expect(ForgeMock, :job_status, fn :client, "astro/app", ^key_id -> status end)
  end

  defp jit_ok do
    expect(ForgeMock, :create_job_runner, fn :client, @scope, name, @labels, nil, "42" ->
      assert name =~ ~r/^mac1-/
      {:ok, {:github_jit, "JIT"}}
    end)

    stub(ForgeMock, :scope_url, fn :client, @scope -> "https://github.com/astro" end)
  end

  defp expect_cleanup do
    expect(AgentDispatchMock, :release_slot, fn "a1", _name -> :ok end)
    expect(ForgeMock, :remove_runner, fn :client, @scope, _name -> :ok end)
  end

  test "a queued job gets a JIT runner, its row written before CreateRunner (J2, F3, G6)" do
    key = job("42")
    happy_path()
    jit_ok()

    expect(AgentDispatchMock, :dispatch_create, fn "a1", %CreateRunnerCommand{} = cmd ->
      assert %{jit_config: "JIT", registration_token: "", label_set_id: ""} = cmd
      assert cmd.labels == @labels
      assert cmd.runner_scope_url == "https://github.com/astro"
      row = Runners.get_runner(cmd.vm_name)
      assert row, "row must exist before CreateRunner is sent"
      assert %{job_key: ^key, agent_id: "a1", repository: "astro/app", job_name: "build"} = row
      :ok
    end)

    assert OnDemand.process_pending(@ctx) == %{created: 1}
    # job stays pending until its runner finishes
    assert Jobs.get_pending(key)
    assert [%{job_key: ^key}] = Runners.list_runners()
  end

  test "a job that already has a runner is skipped" do
    key = job("42")

    {:ok, _} =
      Runners.add_runner(%{
        runner_name: "mac1-00000001",
        agent_id: "a1",
        vm_name: "mac1-00000001",
        runner_scope: @scope,
        forge: "github",
        job_key: key
      })

    assert OnDemand.process_pending(@ctx) == %{created: 0}
  end

  test "a job at max retries is abandoned without trying" do
    key = job("42")
    for _ <- 1..3, do: Jobs.record_retry(key, nil)
    assert OnDemand.process_pending(@ctx) == %{created: 0}
    refute Jobs.get_pending(key)
  end

  test "no available agent tries the next job and doesn't bump retry (J3)" do
    k1 = job("41", %{agent_labels: ["windows"]})
    Process.sleep(2)
    k2 = job("42")

    expect(AgentDispatchMock, :select_agent, fn ["windows"], _ ->
      {:error, :no_available_agent}
    end)

    happy_path()
    jit_ok()
    expect(AgentDispatchMock, :dispatch_create, fn "a1", _ -> :ok end)

    assert OnDemand.process_pending(@ctx) == %{created: 1}
    assert retry(k1) == 0
    assert [%{job_key: ^k2}] = Runners.list_runners()
  end

  test "the avoid-list is the job's failed agents (J4)" do
    key = job("42")
    Jobs.record_retry(key, "a2")

    expect(AgentDispatchMock, :select_agent, fn @labels, opts ->
      assert opts[:avoid] == ["a2"]
      {:error, :no_available_agent}
    end)

    OnDemand.process_pending(@ctx)
  end

  test "a job no longer queued on its forge is dropped and its slot released (F2)" do
    for status <- [{:ok, "completed"}, {:ok, "in_progress"}, {:ok, nil}] do
      key = job("42")
      happy_path("42", status)
      expect(AgentDispatchMock, :release_slot, fn "a1", _ -> :ok end)
      assert OnDemand.process_pending(@ctx) == %{created: 0}
      refute Jobs.get_pending(key), "#{inspect(status)} should drop the job"
      assert Runners.list_runners() == []
    end
  end

  test "a waiting job keeps its place without a runner and without a retry (J8)" do
    key = job("42")
    happy_path("42", {:ok, "waiting"})
    expect(AgentDispatchMock, :release_slot, fn "a1", _ -> :ok end)
    assert OnDemand.process_pending(@ctx) == %{created: 0}
    assert retry(key) == 0
    assert Runners.list_runners() == []
  end

  test "a failed status check still creates the runner" do
    job("42")
    happy_path("42", {:error, :timeout})
    jit_ok()
    expect(AgentDispatchMock, :dispatch_create, fn "a1", _ -> :ok end)
    assert OnDemand.process_pending(@ctx) == %{created: 1}
  end

  test "a forge error creating the runner releases the slot and bumps retry with no agent" do
    key = job("42")
    happy_path()
    expect(ForgeMock, :create_job_runner, fn _, _, _, _, _, _ -> {:error, {:http, 500, ""}} end)
    expect(AgentDispatchMock, :release_slot, fn "a1", _ -> :ok end)
    assert OnDemand.process_pending(@ctx) == %{created: 0}
    assert %{retry_count: 1, failed_agents: []} = Jobs.get_pending(key)
    assert Runners.list_runners() == []
  end

  test "an unknown runner group fails loudly instead of using group 1 (D9)" do
    key = job("42", %{runner_group: "nope"})
    happy_path()

    expect(ForgeMock, :create_job_runner, fn _, _, _, _, "nope", _ ->
      {:error, {:unknown_runner_group, "nope"}}
    end)

    expect(AgentDispatchMock, :release_slot, fn "a1", _ -> :ok end)
    OnDemand.process_pending(@ctx)
    assert retry(key) == 1
  end

  test "a job for an unconfigured forge bumps its retry" do
    {:ok, :inserted} =
      Jobs.add_pending(%{job_key: "gone:42", forge: "gone", runner_scope: @scope})

    assert OnDemand.process_pending(@ctx) == %{created: 0}
    assert retry("gone:42") == 1
  end

  describe "agent replies (F4, F7)" do
    setup do
      key = job("42")
      happy_path()
      jit_ok()
      {:ok, key: key}
    end

    test "a rejection cleans up and reprocesses without a retry", %{key: key} do
      expect(AgentDispatchMock, :dispatch_create, fn _, _ -> {:error, {:rejected, "full"}} end)
      expect_cleanup()
      OnDemand.process_pending(@ctx)
      assert retry(key) == 0
      assert Runners.list_runners() == []
      assert_receive :reprocess
    end

    test "a timeout cleans up and retries with the agent avoided", %{key: key} do
      expect(AgentDispatchMock, :dispatch_create, fn _, _ -> {:error, :timeout} end)
      expect_cleanup()
      OnDemand.process_pending(@ctx)
      assert %{retry_count: 1, failed_agents: ["a1"]} = Jobs.get_pending(key)
      assert Runners.list_runners() == []
    end

    test "the third failure abandons the job", %{key: key} do
      Jobs.record_retry(key, nil)
      Jobs.record_retry(key, nil)
      expect(AgentDispatchMock, :dispatch_create, fn _, _ -> {:error, :disconnected} end)
      expect_cleanup()
      OnDemand.process_pending(@ctx)
      refute Jobs.get_pending(key)
    end

    test "a legacy success whose job is done removes the job", %{key: key} do
      expect(AgentDispatchMock, :dispatch_create, fn _, _ ->
        {:ok, %CommandResult{success: true}}
      end)

      expect(ForgeMock, :job_status, fn _, "astro/app", "42" -> {:ok, "completed"} end)
      expect_cleanup()
      OnDemand.process_pending(@ctx)
      refute Jobs.get_pending(key)
      assert Runners.list_runners() == []
    end

    test "a legacy success whose job is still queued keeps it and reprovisions", %{key: key} do
      expect(AgentDispatchMock, :dispatch_create, fn _, _ ->
        {:ok, %CommandResult{success: true}}
      end)

      expect(ForgeMock, :job_status, fn _, _, _ -> {:ok, "queued"} end)
      expect_cleanup()
      OnDemand.process_pending(@ctx)
      assert %{retry_count: 0} = Jobs.get_pending(key)
      assert Runners.list_runners() == []
      assert_receive :reprocess
    end

    test "a legacy success with an unverifiable job keeps it", %{key: key} do
      expect(AgentDispatchMock, :dispatch_create, fn _, _ ->
        {:ok, %CommandResult{success: true}}
      end)

      expect(ForgeMock, :job_status, fn _, _, _ -> {:error, :timeout} end)
      expect_cleanup()
      OnDemand.process_pending(@ctx)
      assert Jobs.get_pending(key)
      assert_receive :reprocess
    end

    test "a legacy failure retries with the agent avoided", %{key: key} do
      expect(AgentDispatchMock, :dispatch_create, fn _, _ ->
        {:ok, %CommandResult{success: false, error: "vm boot"}}
      end)

      expect_cleanup()
      OnDemand.process_pending(@ctx)
      assert %{retry_count: 1, failed_agents: ["a1"]} = Jobs.get_pending(key)
      assert Runners.list_runners() == []
    end
  end
end
