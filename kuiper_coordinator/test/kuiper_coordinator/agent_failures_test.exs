defmodule KuiperCoordinator.AgentFailuresTest do
  @moduledoc "The failures store (Rust agent_failures.rs): newest first, 100 kept per agent, filters, 24 h counts."
  use KuiperCoordinator.DataCase, async: true

  alias KuiperCoordinator.{AgentFailures, Repo}
  alias KuiperCoordinator.AgentFailures.Failure

  test "records newest first and keeps only the newest 100 per agent" do
    for i <- 1..103,
        do: :ok = AgentFailures.record("a1", :runner_failed, "r#{i}", nil, "fail #{i}")

    :ok = AgentFailures.record("a2", :runner_lost, "x", "github:9", "lost")

    a1 = AgentFailures.recent("a1", 200)
    assert length(a1) == 100
    assert hd(a1).message == "fail 103"
    assert List.last(a1).message == "fail 4"
    assert [%{job_key: "github:9", kind: "runner_lost"}] = AgentFailures.recent("a2", 25)
  end

  test "recent_all filters by kind and agent; nil matches all" do
    :ok = AgentFailures.record("a1", :runner_failed, "r1", nil, "one")
    :ok = AgentFailures.record("a2", :runner_lost, "r2", nil, "two")
    :ok = AgentFailures.record("a1", :runner_lost, "r3", nil, "three")

    assert AgentFailures.recent_all(nil, nil, 10) |> Enum.map(& &1.message) == ~w(three two one)

    assert AgentFailures.recent_all("runner_lost", nil, 10) |> Enum.map(& &1.message) ==
             ~w(three two)

    assert AgentFailures.recent_all("runner_lost", "a1", 10) |> Enum.map(& &1.message) ==
             ~w(three)

    assert AgentFailures.recent_all(nil, nil, 2) |> length() == 2
  end

  test "counts since a time, with unknown kinds counted in the total only" do
    old = DateTime.add(DateTime.utc_now(), -2 * 86_400)

    Repo.insert!(%Failure{
      agent_id: "a1",
      occurred_at: old,
      kind: "runner_failed",
      message: "old"
    })

    Repo.insert!(%Failure{
      agent_id: "a1",
      occurred_at: DateTime.utc_now(),
      kind: "future_kind",
      message: "?"
    })

    :ok = AgentFailures.record("a1", :command_failed, nil, nil, "c")
    :ok = AgentFailures.record("a2", :command_rejected, nil, nil, "full")
    :ok = AgentFailures.record("a2", :command_rejected, nil, nil, "full")

    rows =
      DateTime.utc_now()
      |> DateTime.add(-86_400)
      |> AgentFailures.counts_since()
      |> AgentFailures.count_rows(&(%{"a2" => "zz-host"}[&1] || &1))

    assert [
             %{agent_id: "a1", hostname: "a1", total: 2, counts: [0, 0, 0, 1]},
             %{agent_id: "a2", hostname: "zz-host", total: 2, counts: [0, 0, 2, 0]}
           ] = rows

    assert AgentFailures.label("future_kind") == "Other"
    assert AgentFailures.parse_kind("nope") == nil
  end
end
