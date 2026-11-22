defmodule KuiperCoordinator.Fleet.DrainTest do
  @moduledoc "Draining switched-off agents (Rust fleet.rs drain_unavailable_agents, parity F16)."
  use KuiperCoordinator.DataCase, async: true
  import Mox

  alias KuiperCoordinator.{AgentDispatchMock, ForgeMock, Repo, Runners}
  alias KuiperCoordinator.Fleet.Drain
  alias KuiperCoordinator.Runners.Runner

  setup :verify_on_exit!

  @scope {:org, "astro"}
  @ctx %{forges: %{"github" => {ForgeMock, :gh}}, forge: {ForgeMock, :gh}}
  @now 1_000_000

  defp row(name, agent, attrs \\ %{}) do
    {:ok, r} =
      Runners.add_runner(
        Map.merge(
          %{
            runner_name: name,
            agent_id: agent,
            vm_name: "vm-" <> name,
            runner_scope: @scope,
            forge: "github"
          },
          attrs
        )
      )

    r
  end

  defp age(%Runner{} = r, seconds) do
    at = DateTime.add(DateTime.utc_now(), -seconds, :second)
    r |> Ecto.Changeset.change(inserted_at: at) |> Repo.update!()
  end

  test "no switched-off agents: nothing is looked at and the state clears" do
    expect(AgentDispatchMock, :unavailable_agents, fn -> [] end)
    row("r1", "a1")
    assert Drain.run(%{"r1" => 0}, @ctx, @now) == %{}
  end

  test "an idle pool runner on a switched-off agent is unregistered, then its VM destroyed" do
    expect(AgentDispatchMock, :unavailable_agents, fn -> ["a1"] end)
    row("r1", "a1")
    expect(ForgeMock, :remove_idle_runner, fn :gh, @scope, "r1" -> {:ok, :removed} end)
    expect(AgentDispatchMock, :dispatch_destroy, fn "a1", "vm-r1" -> :ok end)

    assert Drain.run(%{}, @ctx, @now) == %{"r1" => @now}
  end

  test "a busy runner finishes its job: no DestroyRunner" do
    expect(AgentDispatchMock, :unavailable_agents, fn -> ["a1"] end)
    row("r1", "a1")
    expect(ForgeMock, :remove_idle_runner, fn :gh, @scope, "r1" -> {:ok, :busy} end)

    assert Drain.run(%{}, @ctx, @now) == %{}
  end

  test "an unregistered runner is kept while it may still be booting, destroyed after 10 min" do
    expect(AgentDispatchMock, :unavailable_agents, 2, fn -> ["a1"] end)
    row("young", "a1")
    "old" |> row("a1") |> age(11 * 60)
    stub(ForgeMock, :remove_idle_runner, fn :gh, @scope, _ -> {:ok, :not_registered} end)
    expect(AgentDispatchMock, :dispatch_destroy, fn "a1", "vm-old" -> :ok end)

    assert Drain.run(%{}, @ctx, @now) == %{"old" => @now}
    # the young one, a minute from now, is still within the grace
    expect(AgentDispatchMock, :dispatch_destroy, 0, fn _, _ -> :ok end)
    assert Drain.run(%{"old" => @now}, @ctx, @now + 60_000) == %{"old" => @now}
  end

  test "job runners and runners on available agents are left alone" do
    expect(AgentDispatchMock, :unavailable_agents, fn -> ["a1"] end)
    row("job", "a1", %{job_key: "github:42"})
    row("other", "a2")
    # no remove_idle_runner expectation: Mox fails the test if it's called

    assert Drain.run(%{}, @ctx, @now) == %{}
  end

  test "DestroyRunner is sent again only after 5 min without the destroyed event" do
    expect(AgentDispatchMock, :unavailable_agents, 3, fn -> ["a1"] end)
    row("r1", "a1")
    stub(ForgeMock, :remove_idle_runner, fn _, _, _ -> {:ok, :removed} end)
    expect(AgentDispatchMock, :dispatch_destroy, 2, fn "a1", "vm-r1" -> :ok end)

    s = Drain.run(%{}, @ctx, @now)
    assert Drain.run(s, @ctx, @now + 299_000) == %{"r1" => @now}
    assert Drain.run(s, @ctx, @now + 300_000) == %{"r1" => @now + 300_000}
  end

  test "runners that are gone drop out of the state" do
    expect(AgentDispatchMock, :unavailable_agents, fn -> ["a1"] end)
    assert Drain.run(%{"gone" => @now}, @ctx, @now) == %{}
  end

  test "a forge error or a failed send is retried next pass" do
    expect(AgentDispatchMock, :unavailable_agents, 2, fn -> ["a1"] end)
    row("r1", "a1")
    expect(ForgeMock, :remove_idle_runner, fn _, _, _ -> {:error, :timeout} end)
    assert Drain.run(%{}, @ctx, @now) == %{}

    expect(ForgeMock, :remove_idle_runner, fn _, _, _ -> {:ok, :removed} end)
    expect(AgentDispatchMock, :dispatch_destroy, fn _, _ -> {:error, :not_connected} end)
    assert Drain.run(%{}, @ctx, @now) == %{}
  end

  test "a runner is checked against its own forge" do
    expect(AgentDispatchMock, :unavailable_agents, fn -> ["a1"] end)
    row("fj", "a1", %{forge: "forgejo", runner_scope: :instance})
    ctx = put_in(@ctx, [:forges, "forgejo"], {ForgeMock, :fj})
    expect(ForgeMock, :remove_idle_runner, fn :fj, :instance, "fj" -> {:ok, :busy} end)

    assert Drain.run(%{}, ctx, @now) == %{}
  end

  test "step/2 follows Rust drain_step" do
    assert Drain.step(:removed, 0) == :destroy
    assert Drain.step(:busy, 10_000) == :keep
    assert Drain.step(:not_registered, 599) == :keep
    assert Drain.step(:not_registered, 600) == :destroy
  end
end
