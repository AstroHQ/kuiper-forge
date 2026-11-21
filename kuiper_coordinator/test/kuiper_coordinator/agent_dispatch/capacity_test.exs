defmodule KuiperCoordinator.AgentDispatch.CapacityTest do
  use ExUnit.Case, async: true

  alias Cirunner.Agent.{AgentStatus, CapacityLimit, LabelSet}
  alias KuiperCoordinator.AgentDispatch.Capacity
  alias KuiperCoordinator.Fleet.PoolDefinition

  # Ports of the unit tests in kuiper-forge/src/agent_registry.rs. Where the Rust test registers an
  # agent and then calls set_capacity / update_status, the Elixir test builds one status with
  # everything in it (update_status refreshes limits and pools from the status too).

  defp limit(name, max, external \\ 0, active \\ 0),
    do: %CapacityLimit{name: name, max: max, external: external, active: active}

  defp set(labels, opts \\ []) do
    %LabelSet{
      labels: labels,
      limits: Keyword.get(opts, :limits, []),
      pool_size: Keyword.get(opts, :pool),
      id: Keyword.get(opts, :id, ""),
      is_default: Keyword.get(opts, :default, false)
    }
  end

  defp status(max_vms, active, labels, opts \\ []) do
    %AgentStatus{
      max_vms: max_vms,
      active_vms: active,
      labels: labels,
      label_sets: Keyword.get(opts, :sets, []),
      limits: Keyword.get(opts, :limits, []),
      runner_kinds: Keyword.get(opts, :kinds, [])
    }
  end

  defp agent(id, max_vms, active, labels, opts \\ []),
    do: Capacity.from_status(id, status(max_vms, active, labels, opts))

  defp reserve!(cap, labels, set_id, name, now \\ 0) do
    assert {:ok, cap} = Capacity.reserve(cap, labels, set_id, name, now)
    cap
  end

  # A tart agent with a linux base image and a macOS mapping, both pooled, with ids like new agents send.
  defp linux_base_macos_mapping(external_macos) do
    agent("agent_1", 5, 0, ["self-hosted"],
      sets: [
        set(["self-hosted", "macos"], limits: ["macos", "total"], id: "sequoia"),
        set(["self-hosted"], limits: ["total"], id: "noble", default: true)
      ],
      limits: [limit("macos", 2, external_macos), limit("total", 5, external_macos)]
    )
  end

  defp mac_agent(max_vms, active), do: agent("agent_1", max_vms, active, ["macos"])

  # A status from agent_1 with these VMs running, then settle.
  defp mac_status(cap, max_vms, vms) do
    cap
    |> Capacity.update_status(status(max_vms, length(vms), ["macos"]))
    |> Capacity.settle(vms)
  end

  test "test_limits_with_external_vms_reduce_capacity" do
    labels = ["macos"]
    build = fn limits -> agent("agent_1", 2, 0, ["macos"], limits: limits) end

    cap = build.([limit("macos", 2, 1), limit("total", 5, 1)])
    assert Capacity.total_capacity([cap], labels) == 1

    cap = build.([limit("macos", 2, 1), limit("total", 5, 5)])
    assert Capacity.total_capacity([cap], labels) == 0
    assert Capacity.select([cap], labels) == :none

    cap = build.([limit("macos", 2, 0), limit("total", 5, 0)])
    assert Capacity.total_capacity([cap], labels) == 2
  end

  test "test_linux_jobs_skip_macos_limit" do
    sets = [
      set(["self-hosted", "macos"], limits: ["macos", "total"]),
      set(["self-hosted", "linux"], limits: ["total"])
    ]

    build = fn active, macos_active, total_active ->
      agent("agent_1", 4, active, ["self-hosted"],
        sets: sets,
        limits: [limit("macos", 2, 0, macos_active), limit("total", 4, 0, total_active)]
      )
    end

    macos = ["macos"]
    linux = ["linux"]
    cap = build.(2, 2, 2)

    assert Capacity.total_capacity([cap], macos) == 0
    assert Capacity.total_capacity([cap], linux) == 2
    assert Capacity.select([cap], macos) == :none

    # labels matching both sets count against both
    assert Capacity.total_capacity([cap], ["self-hosted"]) == 0

    # a pending linux reservation only holds a slot on the limits linux VMs use
    cap = build.(1, 1, 1) |> reserve!(linux, "", "r1")
    assert Capacity.total_capacity([cap], macos) == 1
    assert Capacity.total_capacity([cap], linux) == 2

    # it becomes a VM: the status lists it and counts it in total active
    cap =
      cap
      |> Capacity.update_status(
        status(4, 2, ["self-hosted"],
          sets: sets,
          limits: [limit("macos", 2, 0, 1), limit("total", 4, 0, 2)]
        )
      )
      |> Capacity.settle(["r1"])

    assert Capacity.total_capacity([cap], macos) == 1
    assert Capacity.total_capacity([cap], linux) == 2

    # a mac and a linux reservation, then the mac command is rejected
    cap = cap |> reserve!(macos, "", "r2") |> reserve!(linux, "", "r3")
    assert Capacity.total_capacity([cap], macos) == 0
    cap = Capacity.release(cap, "r2")
    assert Capacity.total_capacity([cap], macos) == 1
    assert Capacity.total_capacity([cap], linux) == 1
  end

  test "test_base_label_jobs_use_the_base_image_limits" do
    cap = linux_base_macos_mapping(0)
    base = ["self-hosted"]

    assert [%{target_count: 5} | _] = Capacity.pool_definitions([cap])
    assert Capacity.total_capacity([cap], base) == 5
    assert Capacity.total_capacity([cap], ["self-hosted", "macos"]) == 2
  end

  test "test_default_set_is_the_flagged_one" do
    base = ["self-hosted"]

    # a linux mapping with only base labels has the same labels as the macOS default
    cap =
      agent("agent_1", 5, 0, base,
        sets: [
          set(base, limits: ["total"], id: "noble"),
          set(base, limits: ["macos", "total"], id: "sequoia", default: true)
        ],
        limits: [limit("macos", 2), limit("total", 5)]
      )

    assert [%{target_count: 2} | _] = Capacity.pool_definitions([cap])
    assert Capacity.total_capacity([cap], base) == 2

    # the mapping is still reachable by its id
    assert Capacity.available_capacity(cap, base, "noble") == 5
  end

  test "test_display_label_sets_hides_default_only_with_mappings" do
    cap = linux_base_macos_mapping(0)
    assert length(cap.label_sets) == 2
    assert Capacity.display_label_sets(cap) == [["self-hosted", "macos"]]

    base = ["self-hosted"]
    cap = agent("agent_1", 3, 0, base, sets: [set(base, default: true)])
    assert Capacity.display_label_sets(cap) == [base]
  end

  test "test_pool_set_id_picks_that_sets_limits" do
    # two external macOS VMs use up the macOS limit
    cap = linux_base_macos_mapping(2)
    base = ["self-hosted"]

    assert Capacity.available_capacity(cap, base, "noble") == 3
    assert Capacity.available_capacity(cap, base, "sequoia") == 0
    assert Capacity.reserve(cap, base, "sequoia", "r3", 0) == :error
    assert {:ok, _} = Capacity.reserve(cap, base, "noble", "r4", 0)
  end

  test "test_agent_capacity" do
    macos = ["macos"]
    cap = agent("agent_1", 2, 0, macos)
    assert Capacity.total_capacity([cap], macos) == 2

    cap = Capacity.update_status(cap, status(2, 2, macos))
    assert Capacity.total_capacity([cap], macos) == 0
    assert Capacity.select([cap], macos) == :none
  end

  test "test_reserved_slots_preserved_on_status_update" do
    cap = mac_agent(2, 0)
    macos = ["macos"]

    cap = cap |> reserve!([], "", "r1") |> reserve!([], "", "r2")
    assert Capacity.total_capacity([cap], macos) == 0

    cap = mac_status(cap, 2, [])
    assert Capacity.total_capacity([cap], macos) == 0

    cap = mac_status(cap, 2, ["r1"])
    assert Capacity.total_capacity([cap], macos) == 0

    cap = mac_status(cap, 2, ["r1", "r2"])
    assert Capacity.total_capacity([cap], macos) == 0
    cap = mac_status(cap, 2, [])
    assert Capacity.total_capacity([cap], macos) == 2
  end

  test "test_release_after_vm_started_keeps_other_reservations" do
    cap = mac_agent(3, 0)
    macos = ["macos"]

    cap = cap |> reserve!([], "", "r1") |> mac_status(3, ["r1"])
    cap = cap |> reserve!([], "", "r2") |> reserve!([], "", "r3")
    assert Capacity.total_capacity([cap], macos) == 0

    # r1's job finishes: releasing it must not free r2's or r3's slot
    cap = cap |> mac_status(3, []) |> Capacity.release("r1")
    assert Capacity.total_capacity([cap], macos) == 1
  end

  test "test_get_pool_definitions" do
    caps = [
      agent("agent_1", 2, 0, ["self-hosted", "macos", "arm64"]),
      agent("agent_2", 3, 0, ["self-hosted", "macos", "arm64"]),
      agent("agent_3", 5, 0, ["self-hosted", "linux", "x64"])
    ]

    pools = Capacity.pool_definitions(caps)
    assert length(pools) == 2

    macos_pool = Enum.find(pools, &("macos" in &1.labels))
    assert macos_pool.target_count == 5
    assert macos_pool.members == ["agent_1", "agent_2"]

    linux_pool = Enum.find(pools, &("linux" in &1.labels))
    assert linux_pool.target_count == 5
    assert linux_pool.members == ["agent_3"]
  end

  test "test_explicit_pools_per_label_set" do
    explicit =
      agent("explicit", 4, 0, ["self-hosted"],
        sets: [
          set(["self-hosted", "macOS"], pool: 1, id: "sequoia"),
          set(["self-hosted", "linux"], pool: 2, id: "noble"),
          set(["self-hosted", "windows"], pool: 0, id: "win")
        ]
      )

    legacy =
      agent("legacy", 2, 0, ["self-hosted", "macos"], sets: [set(["self-hosted", "macos"])])

    pools =
      [explicit, legacy]
      |> Capacity.pool_definitions()
      |> Enum.sort_by(&{&1.agent_id, &1.labels})

    pool = fn labels, target, id ->
      %PoolDefinition{
        labels: labels,
        target_count: target,
        agent_id: "explicit",
        label_set_id: id,
        members: ["explicit"]
      }
    end

    # the size-0 set gets no pool, and the explicit agent isn't in the legacy pool
    assert pools == [
             %PoolDefinition{
               labels: ["macos", "self-hosted"],
               target_count: 2,
               agent_id: nil,
               label_set_id: "",
               members: ["legacy"]
             },
             pool.(["linux", "self-hosted"], 2, "noble"),
             pool.(["macos", "self-hosted"], 1, "sequoia")
           ]

    assert PoolDefinition.key(Enum.at(pools, 0)) == "macos,self-hosted"
    assert PoolDefinition.key(Enum.at(pools, 1)) == "linux,self-hosted@noble"
  end

  test "test_legacy_pool_skips_explicit_pool_agents" do
    base = ["self-hosted"]

    explicit =
      agent("explicit", 4, 0, base,
        sets: [set(["self-hosted", "linux"], pool: 1), set(base, pool: 0)]
      )

    legacy = agent("legacy", 1, 0, base, sets: [set(base)])
    caps = [explicit, legacy]
    in_pool = &Capacity.in_legacy_pool?(&1, base)

    assert Capacity.legacy_pool_capacity(caps, base) == 1
    assert Capacity.select(caps, base, include: in_pool) == {:ok, "legacy"}

    legacy = reserve!(legacy, base, "", "r7")
    assert Capacity.select([explicit, legacy], base, include: in_pool) == :none
  end

  test "test_legacy_pool_target_follows_base_image_limits" do
    base = ["self-hosted"]

    # macOS base image plus a linux mapping: max_vms is the total, base-label runners are macOS
    cap =
      agent("agent_1", 5, 0, base,
        sets: [
          set(["self-hosted", "linux"], limits: ["total"]),
          set(base, limits: ["macos", "total"])
        ],
        limits: [limit("macos", 2), limit("total", 5)]
      )

    pools = Capacity.pool_definitions([cap])
    assert length(pools) == 1
    assert hd(pools).target_count == 2
    assert Capacity.legacy_pool_capacity([cap], base) == 2
  end

  test "test_get_pool_definitions_empty" do
    assert Capacity.pool_definitions([]) == []
  end

  test "test_reconcile_reservations" do
    cap = mac_agent(3, 1)
    macos = ["macos"]

    cap = cap |> reserve!([], "", "r1", 0) |> reserve!([], "", "r2", 0)
    assert Capacity.total_capacity([cap], macos) == 0

    # too new to judge by the DB
    cap = Capacity.reconcile(cap, ["r1"], 60_000, 0)
    assert Capacity.total_capacity([cap], macos) == 0

    # r2's runner record is gone
    cap = Capacity.reconcile(cap, ["r1"], 0, 0)
    assert Capacity.total_capacity([cap], macos) == 1
  end

  test "test_label_sets_matching" do
    cap =
      agent("agent_1", 2, 0, ["self-hosted", "macos"],
        sets: [
          set(["self-hosted", "macos", "sequoia"]),
          set(["self-hosted", "macos", "ventura"])
        ]
      )

    # case-insensitive
    assert Capacity.select([cap], ["self-hosted", "macOS", "sequoia"]) == {:ok, "agent_1"}
    assert Capacity.select([cap], ["self-hosted", "macos", "ventura"]) == {:ok, "agent_1"}
    # no single capability has both
    assert Capacity.select([cap], ["self-hosted", "macos", "sequoia", "ventura"]) == :none
    assert Capacity.select([cap], ["self-hosted", "macos"]) == {:ok, "agent_1"}
    assert Capacity.matches_labels?(cap, ["SELF-HOSTED"])
  end

  test "test_select_agent_prefers_least_loaded" do
    busy = agent("busy", 2, 1, ["macos"])
    idle = agent("idle", 2, 0, ["macos"])
    labels = ["macos"]

    assert Capacity.select([busy, idle], labels) == {:ok, "idle"}

    # a reservation counts as load too
    idle = idle |> reserve!([], "", "r13") |> reserve!([], "", "r14")
    assert Capacity.select([busy, idle], labels) == {:ok, "busy"}
  end

  test "test_select_agent_for_kind_needs_advertised_kind" do
    a = agent("a", 2, 0, ["linux"])
    labels = ["linux"]

    for_kind = fn caps, kind ->
      Capacity.select(caps, labels, include: &Capacity.runs?(&1, kind))
    end

    # nobody advertises forgejo yet (older agents)
    assert for_kind.([a, agent("b", 2, 0, ["linux"])], "forgejo") == :none

    b = agent("b", 2, 0, ["linux"], kinds: ["forgejo"])
    assert for_kind.([a, b], "forgejo") == {:ok, "b"}
    # every agent runs GitHub's runner
    assert for_kind.([a, b], "github") == {:ok, "a"}
  end

  test "test_unavailable_agent_gets_no_new_runners" do
    labels = ["macos"]
    a1 = agent("a1", 2, 0, labels)
    a2 = agent("a2", 2, 0, labels)
    assert Capacity.total_capacity([a1, a2], labels) == 4

    a1 = %{a1 | unavailable: true}
    assert Capacity.total_capacity([a1, a2], labels) == 2
    assert Capacity.available_capacity(a1, labels) == 0
    assert Capacity.reserve(a1, labels, "", "r1", 0) == :error
    # still matches, so the dashboard can say why a job waits
    assert Enum.all?([a1, a2], &Capacity.matches_labels?(&1, labels))
    # even when it's the only agent left, unlike one that failed the job
    assert Capacity.select([a1, a2], labels, avoid: ["a2"]) == {:ok, "a2"}

    pools = Capacity.pool_definitions([a1, a2])
    assert length(pools) == 1
    assert hd(pools).target_count == 2

    a2 = %{a2 | unavailable: true}
    assert Capacity.select([a1, a2], labels) == :none
    assert Capacity.pool_definitions([a1, a2]) == []

    a1 = %{a1 | unavailable: false}
    assert Capacity.select([a1, a2], labels) == {:ok, "a1"}
  end

  test "test_select_agent_avoids_failed_agent_when_possible" do
    a = agent("a", 2, 0, ["macos"])
    b = agent("b", 2, 0, ["macos"])
    labels = ["macos"]

    # tie on load, ids break it: "a" wins unless avoided
    assert Capacity.select([a, b], labels) == {:ok, "a"}
    assert Capacity.select([a, b], labels, avoid: ["a"]) == {:ok, "b"}
    # everyone failed: still hand out someone rather than strand the job
    assert Capacity.select([a, b], labels, avoid: ["a", "b"]) == {:ok, "a"}

    # a full non-failed agent doesn't count as an alternative
    b = b |> reserve!([], "", "r15") |> reserve!([], "", "r16")
    assert Capacity.select([a, b], labels, avoid: ["a"]) == {:ok, "a"}
  end
end
