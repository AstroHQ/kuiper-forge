defmodule KuiperCoordinator.Fleet.IdleTest do
  # Rust fleet.rs check_idle_runners (parity F6, F13) plus the fixes for open issues #49 (F14:
  # the idle clock starts at Started, not creation) and #48 (F15: runners without a repository
  # are looked up in their scope). Nye's Forgejo read (Nov 21), item 7.
  use KuiperCoordinator.DataCase, async: false
  import Mox

  alias Cirunner.Agent.RunnerEvent
  alias KuiperCoordinator.{AgentDispatchMock, ForgeMock, Jobs, Repo, Runners}
  alias KuiperCoordinator.Fleet.{Idle, Manager}
  alias KuiperCoordinator.Runners.Runner

  setup :verify_on_exit!

  @repo_scope {:repo, "lodge", "app"}
  @org_scope {:org, "lodge"}
  @ctx %{forges: %{"fj" => {ForgeMock, :fj}, "gh" => {ForgeMock, :gh}}, async: false}

  setup do
    stub(ForgeMock, :runner_kind, fn
      :fj -> "forgejo"
      :gh -> "github"
    end)

    :ok
  end

  defp job(handle, attrs \\ %{}) do
    {:ok, _} =
      Jobs.add_pending(
        Map.merge(
          %{
            job_key: "fj:#{handle}",
            forge: "fj",
            runner_scope: @repo_scope,
            repository: "lodge/app"
          },
          attrs
        )
      )

    "fj:#{handle}"
  end

  # A runner row created `age_s` seconds ago.
  defp runner(name, key, attrs, age_s) do
    {:ok, _} =
      Runners.add_runner(
        Map.merge(
          %{
            runner_name: name,
            agent_id: "a1",
            vm_name: name,
            runner_scope: @repo_scope,
            forge: "fj",
            job_key: key,
            repository: "lodge/app"
          },
          attrs
        )
      )

    at = DateTime.add(DateTime.utc_now(), -age_s, :second)
    Repo.update_all(from(r in Runner, where: r.runner_name == ^name), set: [inserted_at: at])
    name
  end

  @now 10_000_000
  # an Idle (120 s timeout) whose runners reported Started `ago_s` seconds before @now
  defp started(names, ago_s) do
    Enum.reduce(names, Idle.new(120), &Idle.runner_started(&2, &1, @now - ago_s * 1000))
  end

  describe "timeout and backoff (Rust config.rs idle_runner_timeout, fleet.rs:2777, 2792)" do
    test "default 600 s, 0 is off, raised to at least 120 s" do
      assert %Idle{timeout_ms: 600_000} = Idle.new(nil)
      assert Idle.new(0) == nil
      assert %Idle{timeout_ms: 120_000} = Idle.new(30)
      assert %Idle{timeout_ms: 900_000} = Idle.new(900)
    end

    test "the action follows the job's status" do
      assert Idle.action({:ok, "in_progress"}) == :started
      assert Idle.action({:ok, "queued"}) == :tear_down
      assert Idle.action({:ok, nil}) == :tear_down
      assert Idle.action({:ok, "completed"}) == :wait
      assert Idle.action({:ok, "waiting"}) == :wait
      assert Idle.action({:error, :timeout}) == :wait
    end

    test "backoff starts at the timeout and doubles up to an hour" do
      t = 600_000
      assert Idle.next_backoff(nil, t) == 600_000
      assert Idle.next_backoff(600_000, t) == 1_200_000
      assert Idle.next_backoff(2_400_000, t) == 3_600_000
      assert Idle.next_backoff(3_600_000, t) == 3_600_000
      assert Idle.next_backoff(nil, 7_200_000) == 3_600_000
    end
  end

  describe "check (F13)" do
    test "a started runner whose job still waits is destroyed; its job backs off, no retry" do
      key = job("h1")
      runner("r1", key, %{}, 300)
      expect(ForgeMock, :job_status, fn :fj, "lodge/app", "h1" -> {:ok, "queued"} end)
      expect(AgentDispatchMock, :dispatch_destroy, fn "a1", "r1" -> :ok end)

      idle = Idle.check(started(["r1"], 130), @ctx, @now)

      assert Map.has_key?(idle.tearing_down, "r1")
      assert Idle.backing_off(idle, @now) == MapSet.new([key])
      assert Idle.backing_off(idle, @now + 120_001) == MapSet.new()
      assert Jobs.get_pending(key).retry_count == 0

      # the destroy isn't sent again within 120 s, then is
      assert Idle.check(idle, @ctx, @now + 60_000) == idle
      expect(ForgeMock, :job_status, fn :fj, "lodge/app", "h1" -> {:ok, nil} end)
      expect(AgentDispatchMock, :dispatch_destroy, fn "a1", "r1" -> :ok end)
      idle = Idle.check(idle, @ctx, @now + 121_000)
      # second teardown of the same job: the delay doubles
      assert {_, 240_000} = idle.backoff[key]
    end

    test "a running job marks the runner started: never checked again" do
      runner("r1", job("h1"), %{}, 300)
      expect(ForgeMock, :job_status, 1, fn :fj, _, "h1" -> {:ok, "in_progress"} end)

      idle = Idle.check(started(["r1"], 130), @ctx, @now)
      assert MapSet.member?(idle.started, "r1")
      assert Idle.check(idle, @ctx, @now + 600_000).started == idle.started
    end

    test "a finishing job or a lookup error waits for the next check" do
      runner("r1", job("h1"), %{}, 300)
      runner("r2", job("h2"), %{}, 300)
      expect(ForgeMock, :job_status, fn :fj, _, "h1" -> {:ok, "completed"} end)
      expect(ForgeMock, :job_status, fn :fj, _, "h2" -> {:error, :timeout} end)

      idle = Idle.check(started(["r1", "r2"], 130), @ctx, @now)
      assert idle.tearing_down == %{} and idle.started == MapSet.new()
    end

    test "GitHub JIT runners and runners without a job are left alone" do
      runner("g1", "gh:1", %{forge: "gh", runner_scope: {:org, "astro"}}, 3000)
      runner("p1", nil, %{pool: "pool"}, 3000)

      idle = Idle.check(started(["g1", "p1"], 3000), @ctx, @now)
      assert idle.tearing_down == %{}
    end

    test "backoff is dropped once the job leaves the queue" do
      key = job("h1")
      runner("r1", key, %{}, 300)
      expect(ForgeMock, :job_status, fn :fj, _, "h1" -> {:ok, "queued"} end)
      expect(AgentDispatchMock, :dispatch_destroy, fn "a1", "r1" -> :ok end)
      idle = Idle.check(started(["r1"], 130), @ctx, @now)

      Jobs.remove_pending(key)
      Runners.remove_runner("r1")
      idle = Idle.check(idle, @ctx, @now + 1000)
      assert idle.backoff == %{} and idle.tearing_down == %{}
    end
  end

  describe "#49 (F14): the idle clock starts at Started" do
    test "a runner that takes 150 s to register under a 120 s timeout is not torn down" do
      runner("r1", job("h1"), %{}, 150)
      # no forge or agent calls: it isn't idle yet
      assert Idle.check(Idle.new(120), @ctx, @now).tearing_down == %{}

      # it starts now: still not idle 100 s later
      idle = Idle.runner_started(Idle.new(120), "r1", @now)
      assert Idle.check(idle, @ctx, @now + 100_000).tearing_down == %{}
    end

    test "a runner that never starts is measured from creation after a 10 min boot grace" do
      runner("r1", job("h1"), %{}, 120 + 600 - 5)
      assert Idle.check(Idle.new(120), @ctx, @now).tearing_down == %{}

      runner("r2", job("h2"), %{}, 120 + 600 + 5)
      expect(ForgeMock, :job_status, fn :fj, _, "h2" -> {:ok, "queued"} end)
      expect(AgentDispatchMock, :dispatch_destroy, fn "a1", "r2" -> :ok end)
      assert Map.keys(Idle.check(Idle.new(120), @ctx, @now).tearing_down) == ["r2"]
    end
  end

  describe "#48 (F15): runners without a repository" do
    test "an org-scoped runner that never gets its job is torn down via its scope's jobs" do
      key = job("h9", %{runner_scope: @org_scope, repository: nil})
      runner("r9", key, %{runner_scope: @org_scope, repository: nil}, 300)
      expect(ForgeMock, :scope_job_status, fn :fj, @org_scope, "h9" -> {:ok, "queued"} end)
      expect(AgentDispatchMock, :dispatch_destroy, fn "a1", "r9" -> :ok end)

      idle = Idle.check(started(["r9"], 130), @ctx, @now)
      assert Map.has_key?(idle.tearing_down, "r9")
    end

    test "the Forgejo client looks the handle up in the scope's job list" do
      name = {__MODULE__, make_ref()}

      Req.Test.stub(name, fn conn ->
        ["api", "v1", "orgs", "lodge", "actions", "runners", "jobs"] = conn.path_info

        Req.Test.json(conn, [
          %{"handle" => "h1", "status" => "waiting"},
          %{"handle" => "h2", "status" => "running"}
        ])
      end)

      {:ok, c} =
        KuiperCoordinator.Forgejo.Client.new(
          url: "http://forgejo.test",
          token: "t",
          scopes: ["lodge"],
          req_options: [plug: {Req.Test, name}, retry: false]
        )

      fj = {KuiperCoordinator.Forgejo.Client, c}
      assert KuiperCoordinator.Forge.scope_job_status(fj, @org_scope, "h1") == {:ok, "queued"}

      assert KuiperCoordinator.Forge.scope_job_status(fj, @org_scope, "h2") ==
               {:ok, "in_progress"}

      assert KuiperCoordinator.Forge.scope_job_status(fj, @org_scope, "h3") == {:ok, nil}
    end
  end

  describe "the fleet manager (F6, F13)" do
    defp manager_state do
      {:ok, state, _} =
        Manager.init(mode: :webhook, forges: @ctx.forges, tick_ms: 3_600_000, async: false)

      state
    end

    defp event(name, type),
      do: %RunnerEvent{runner_name: name, vm_id: name, event_type: type, error: "killed"}

    test "a torn-down runner's events aren't failures, its job backs off, late events are ignored" do
      key = job("h1")
      runner("r1", key, %{}, 300)
      expect(ForgeMock, :job_status, fn :fj, _, "h1" -> {:ok, "queued"} end)
      expect(AgentDispatchMock, :dispatch_destroy, fn "a1", "r1" -> :ok end)

      now = System.monotonic_time(:millisecond)
      idle = Idle.runner_started(Idle.new(120), "r1", now - 130_000)
      state = %{manager_state() | idle: Idle.check(idle, @ctx, now)}

      # the agent reports the killed job and the destroy: slot and row go, no retry, and the
      # pass that follows gives the backing-off job no runner (no select_agent expected)
      expect(AgentDispatchMock, :release_slot, fn "a1", "r1" -> :ok end)
      expect(ForgeMock, :remove_runner, fn :fj, @repo_scope, "r1" -> :ok end)

      {:noreply, state} =
        Manager.handle_info({:agent_event, "a1", {:runner_event, event("r1", :FAILED)}}, state)

      {:noreply, _state} =
        Manager.handle_info({:agent_event, "a1", {:runner_event, event("r1", :DESTROYED)}}, state)

      assert Runners.get_runner("r1") == nil
      assert %{retry_count: 0, failed_agents: []} = Jobs.get_pending(key)
    end

    test "Started starts the clock" do
      state = manager_state()

      {:noreply, state} =
        Manager.handle_info({:agent_event, "a1", {:runner_event, event("r1", :STARTED)}}, state)

      assert Map.has_key?(state.idle.active_since, "r1")
    end

    test "off with a 0 timeout" do
      {:ok, s, _} = Manager.init(mode: :webhook, forges: @ctx.forges, idle_runner_timeout_s: 0)
      assert s.idle == nil
    end
  end
end
