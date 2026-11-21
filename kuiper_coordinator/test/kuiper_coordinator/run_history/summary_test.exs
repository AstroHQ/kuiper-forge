defmodule KuiperCoordinator.RunHistory.SummaryTest do
  # Ported from the Rust coordinator's metrics/summary.rs tests (parity MX6).
  use ExUnit.Case, async: true

  alias KuiperCoordinator.RunHistory.{JobRun, RunnerRun, Summary}

  @hour 3_600_000
  @day 24 * @hour

  defp run(agent, requested, outcome) do
    %RunnerRun{
      runner_name: "#{agent}-#{requested}",
      agent_id: agent,
      hostname: "#{agent}.local",
      agent_type: "tart",
      requested_at: requested,
      outcome: outcome
    }
  end

  test "nearest-rank percentiles" do
    v = Enum.to_list(1..100)
    assert Summary.percentile(v, 0.5) == 50
    assert Summary.percentile(v, 0.95) == 95
    assert Summary.percentile([7], 0.95) == 7
    assert Summary.percentile([], 0.5) == nil
  end

  test "success ignores rejections and our own teardowns" do
    runs = [
      run("a", 0, "completed"),
      run("a", 1, "completed"),
      run("a", 2, "completed"),
      run("a", 3, "lost"),
      run("a", 4, "rejected"),
      run("a", 5, "idled"),
      run("a", 6, nil)
    ]

    [g] = Summary.by_agent(runs)
    assert g.runs == 7
    assert {g.completed, g.failures, g.rejected, g.stopped, g.running} == {3, 1, 1, 1, 1}
    assert g.success_pct == 75.0
    assert g.failed_phases == [{"unknown", 1}]
  end

  test "command_failed counts as a failure; drained and destroyed as stopped" do
    runs = [
      run("a", 0, "command_failed"),
      run("a", 1, "drained"),
      run("a", 2, "destroyed"),
      %{run("a", 3, "failed") | failed_phase: "boot"},
      %{run("a", 4, "failed") | failed_phase: "boot"}
    ]

    [g] = Summary.by_agent(runs)
    assert {g.failures, g.stopped, g.success_pct} == {3, 2, 0.0}
    assert g.failed_phases == [{"boot", 2}, {"unknown", 1}]
    assert Summary.total([]).success_pct == nil
  end

  test "missing steps are skipped per step" do
    old = %{run("a", 0, "completed") | started_at: 30_000}
    new = %{run("a", 1_000, "completed") | started_at: 21_000, steps: %{"boot" => 15_000}}
    [g] = Summary.by_agent([old, new])
    assert g.provision.count == 2
    assert g.provision.p95 == 30_000
    assert Summary.step(g, "boot").ran.count == 1
    assert Summary.step(g, "create").ran.count == 0
    assert Summary.step(g, "create").ran.p50 == nil
  end

  test "negative spans are dropped" do
    r = %{run("a", 10_000, "completed") | acked_at: 9_000, started_at: 12_000}
    [g] = Summary.by_agent([r])
    assert g.ack.count == 0
    assert g.provision.p50 == 2_000
  end

  test "groups sort busiest first and keep the newest hostname" do
    runs = [
      run("b", 0, "completed"),
      %{run("a", 1, "completed") | hostname: "old.local"},
      %{run("a", 2, "completed") | hostname: "new.local"}
    ]

    assert [%{key: "a", label: "new.local"}, %{key: "b"}] = Summary.by_agent(runs)
  end

  test "by_image falls back to labels" do
    a = %{run("a", 0, "completed") | image: "macos-15"}
    b = %{run("b", 0, "completed") | labels: ["linux", "x64"]}
    keys = Summary.by_image([a, b]) |> Enum.map(& &1.key)
    assert "macos-15" in keys
    assert "labels: linux, x64" in keys
  end

  test "job breakdown uses the runner that ran it" do
    failed = %{run("a", 10_000, "failed") | job_key: "github:1", started_at: 11_000}
    ok = %{run("b", 20_000, "completed") | job_key: "github:1", started_at: 50_000}

    job = %JobRun{
      job_key: "github:1",
      queued_at: 1_000,
      seen_at: 3_000,
      started_at: 52_000,
      runner_name: ok.runner_name,
      attempts: 2,
      outcome: "ran"
    }

    s = Summary.jobs([job], [failed, ok])
    assert s.ran == 1
    assert s.mean_attempts == 2.0
    assert s.wait.p50 == 51_000
    assert s.to_coordinator.p50 == 2_000
    assert s.dispatch.p50 == 17_000
    assert s.provision.p50 == 30_000
    assert s.pickup.p50 == 2_000
  end

  test "polling jobs with no runner name use the runner that completed them" do
    ok = %{run("b", 20_000, "completed") | job_key: "forgejo:9", started_at: 25_000}

    job = %JobRun{
      job_key: "forgejo:9",
      queued_at: 5_000,
      seen_at: 5_000,
      attempts: 1,
      outcome: "cancelled"
    }

    s = Summary.jobs([job], [ok])
    assert {s.cancelled, s.to_coordinator.count, s.dispatch.p50} == {1, 0, 15_000}
    assert Summary.jobs([], []).mean_attempts == nil
  end

  test "buckets" do
    now = 10 * @day + 5 * @hour
    starts = Summary.bucket_starts(now - Summary.range_ms("24h"), now, @hour)
    assert length(starts) == 25
    assert hd(starts) == 9 * @day + 5 * @hour
    assert List.last(starts) == now

    runs = [run("a", now - 30 * 60_000, "completed"), run("a", now - 2 * @day, "completed")]
    agents = Summary.by_agent(runs)
    c = Summary.charts(runs, [], agents, Summary.step_names(runs), "24h", now)
    assert length(c.buckets) == 25
    assert Enum.sum(c.outcomes.completed) == 1
    assert Enum.at(c.outcomes.completed, 23) == 1
    assert length(c.provision) == 1
    assert hd(c.buckets) == "Jan 10 05:00"
  end

  test "ranges parse, unknown is 7d" do
    assert Summary.parse_range("90d") == "90d"
    assert Summary.parse_range("bogus") == "7d"
    assert Summary.parse_range(nil) == "7d"
    assert Summary.range_ms("30d") == 30 * @day
  end

  test "skipped steps" do
    runs =
      [0, 0, 0, 20_000]
      |> Enum.with_index()
      |> Enum.map(fn {install, i} ->
        %{run("a", i, "completed") | steps: %{"install" => install}}
      end)

    install = Summary.step(hd(Summary.by_agent(runs)), "install")
    assert install.skipped == 3
    assert Summary.skipped_pct(install) == 75.0
    assert install.ran.p50 == 20_000, "a download's time leaves out the cached runs"
    assert install.typical.p50 == 0
  end

  defp install_run(image, source, install) do
    steps = if install, do: %{"install" => install}, else: %{}
    %{run("a", 0, "completed") | image: image, install_source: source, steps: steps}
  end

  test "install sources" do
    runs = [
      install_run("linux", "hit", 3_000),
      install_run("linux", "hit", 5_000),
      install_run("linux", "filled", 9_000),
      install_run("linux", nil, 40_000),
      install_run("linux", "fell_back", nil),
      install_run("mac", nil, 0),
      install_run("mac", nil, nil)
    ]

    got =
      Summary.by_install(runs)
      |> Enum.map(&{&1.image, &1.source, &1.runs, &1.install.count, &1.install.p50})

    assert got == [
             {"linux", "hit", 2, 2, 3_000},
             {"linux", "filled", 1, 1, 9_000},
             {"linux", "fell_back", 1, 0, nil},
             {"linux", "not_recorded", 1, 1, 40_000},
             {"mac", "image", 1, 0, nil}
           ]
  end

  test "unknown install source is kept and sorts last" do
    runs = [install_run("linux", "zz-newer-agent", 1_000), install_run("linux", nil, 0)]
    assert Enum.map(Summary.by_install(runs), & &1.source) == ["image", "zz-newer-agent"]
  end

  test "step names keep known order then extras" do
    r = %{run("a", 0, nil) | steps: %{"zz-new" => 1, "clock-sync" => 1}}

    assert Summary.step_names([r]) ==
             ~w(create boot install setup job destroy clock-sync zz-new)
  end

  test "fmt_ms" do
    assert Summary.fmt_ms(nil) == "-"
    assert Summary.fmt_ms(850) == "850ms"
    assert Summary.fmt_ms(42_300) == "42.3s"
    assert Summary.fmt_ms(192_000) == "3m 12s"
    assert Summary.fmt_ms(7_500_000) == "2h 5m"
  end
end
