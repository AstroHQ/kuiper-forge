defmodule KuiperCoordinator.Admin.DashboardTest do
  @moduledoc """
  Dashboard data (parity UI2), ported from Rust admin/templates.rs and routes.rs tests:
  split_labels, capacity summing, format_age, and pending jobs sorted waiting-first then oldest
  with matching/unavailable/free counts.
  """
  use KuiperCoordinator.DataCase, async: false

  alias Cirunner.Agent.{AgentStatus, CapacityLimit, LabelSet}
  alias KuiperCoordinator.Admin.Dashboard

  defp status(base, sets, attrs \\ []) do
    struct(
      %AgentStatus{
        labels: base,
        label_sets:
          Enum.map(sets, fn
            {labels, opts} -> struct(%LabelSet{labels: labels}, opts)
            labels -> %LabelSet{labels: labels}
          end)
      },
      attrs
    )
  end

  describe "split_labels/1" do
    test "keeps only what mappings add, leaving out the default set" do
      s =
        status(["Self-Hosted"], [
          {["self-hosted"], is_default: true},
          ["self-hosted", "windows"],
          ["self-hosted", "linux", "ubuntu-24.04"]
        ])

      assert Dashboard.split_labels(s) ==
               {["Self-Hosted"], [["windows"], ["linux", "ubuntu-24.04"]]}
    end

    test "without mappings it's just the base" do
      s = status(["self-hosted", "macos"], [["self-hosted", "macos"]])
      assert Dashboard.split_labels(s) == {["self-hosted", "macos"], []}
      assert Dashboard.split_labels(nil) == {[], []}
    end
  end

  defp summary(online, active, max_vms, limits) do
    %{
      online?: online,
      revoked?: false,
      active_vms: active,
      max_vms: max_vms,
      limits: for({n, u, m, e} <- limits, do: %{name: n, used: u, max: m, external: e})
    }
  end

  test "capacity sums connected agents and their limits" do
    agents = [
      # tart: max_vms is only the macOS cap, the total limit is what the host runs
      summary(true, 1, 2, [{"macos", 1, 2, 0}, {"total", 2, 5, 0}]),
      summary(true, 0, 2, [{"macos", 1, 2, 1}, {"total", 1, 5, 1}]),
      # proxmox reports no limits, max_vms is its total
      summary(true, 2, 4, []),
      # offline agents have no capacity to give
      summary(false, 0, 2, [{"macos", 0, 2, 0}, {"total", 0, 5, 0}]),
      # nor do revoked ones
      %{summary(true, 0, 3, []) | revoked?: true}
    ]

    assert {Dashboard.capacity_used(Enum.at(agents, 0)),
            Dashboard.capacity_max(Enum.at(agents, 0))} ==
             {2, 5}

    assert {Dashboard.capacity_used(Enum.at(agents, 2)),
            Dashboard.capacity_max(Enum.at(agents, 2))} ==
             {2, 4}

    cap = Dashboard.capacity_summary(agents)
    assert {cap.used, cap.total, cap.external} == {5, 14, 1}
    # total is the headline, not repeated as a limit
    assert [%{name: "macos", used: 2, max: 4, external: 1}] = cap.limits
    assert Dashboard.limit_label("macos") == "macOS"
  end

  test "format_age matches Rust" do
    assert Dashboard.format_age(-5) == "0s"
    assert Dashboard.format_age(42) == "42s"
    assert Dashboard.format_age(185) == "3m 5s"
    assert Dashboard.format_age(2 * 3600 + 600) == "2h 10m"
    assert Dashboard.format_age(86_400 + 4 * 3600 + 59) == "1d 4h"
  end

  test "pending jobs: waiting ones first then oldest, with matching, unavailable and free" do
    now = ~U[2026-11-21 12:00:00.000000Z]

    job = fn key, mins, labels ->
      %{
        job_key: key,
        queued_at: DateTime.add(now, -mins * 60, :second),
        inserted_at: now,
        agent_labels: labels,
        job_labels: labels,
        repository: "astro/app",
        workflow_name: "CI",
        job_name: "build",
        retry_count: 0,
        failed_agents: []
      }
    end

    pending = [
      job.("github:new", 1, ["self-hosted", "macos"]),
      job.("github:old-assigned", 30, ["self-hosted", "macos"]),
      job.("github:old", 10, ["self-hosted", "macos"]),
      job.("github:linux", 5, ["Linux"])
    ]

    runners = [%{job_key: "github:old-assigned", agent_id: "mac1"}]

    mac = fn active, limits_attrs ->
      status(["self-hosted"], [{["self-hosted", "macos"], limits: ["macos", "total"]}],
        max_vms: 2,
        active_vms: active,
        limits: limits_attrs
      )
    end

    connected = %{
      # macOS limit 2 with 1 external VM: one slot left though max_vms says 2
      "mac1" => %{
        status: mac.(0, [%CapacityLimit{name: "macos", max: 2, external: 1, active: 0}])
      },
      "mac2" => %{status: mac.(2, [])},
      "mac3" => %{status: mac.(0, [])},
      "lin1" => %{status: status(["self-hosted", "linux"], [], max_vms: 4)}
    }

    agents = [
      %{agent_id: "mac3", unavailable_at: now},
      %{agent_id: "mac1", unavailable_at: nil}
    ]

    rows = Dashboard.pending_jobs(pending, runners, connected, agents, now)

    assert Enum.map(rows, & &1.job_key) ==
             ["github:old", "github:linux", "github:new", "github:old-assigned"]

    [old, linux, _new, assigned] = rows
    assert assigned.assigned_agent == "mac1"
    assert old.waiting == "10m 0s"
    # mac1: 1 free (macOS limit), mac2 full, mac3 unavailable
    assert {old.matching_agents, old.unavailable_agents, old.free_capacity} == {3, 1, 1}
    # flat labels, matched case-insensitively
    assert {linux.matching_agents, linux.free_capacity} == {1, 4}
  end

  test "set_unavailable writes the DB and tells scheduling" do
    Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, "agents")

    Repo.insert!(%KuiperCoordinator.Agents.Agent{
      agent_id: "agent_x",
      hostname: "h",
      agent_type: "tart",
      serial_number: "01",
      expires_at: DateTime.add(DateTime.utc_now(), 86_400, :second)
    })

    assert :ok = KuiperCoordinator.Agents.set_unavailable("agent_x", true)
    assert KuiperCoordinator.Agents.get_agent("agent_x").unavailable_at
    assert_receive {:agent_availability, "agent_x", false}
    assert :ok = KuiperCoordinator.Agents.set_unavailable("agent_x", false)
    refute KuiperCoordinator.Agents.get_agent("agent_x").unavailable_at
    assert_receive {:agent_availability, "agent_x", true}
    assert {:error, :not_found} = KuiperCoordinator.Agents.set_unavailable("nope", true)
  end
end
