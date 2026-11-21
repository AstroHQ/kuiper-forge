defmodule KuiperCoordinator.RunHistoryTest do
  @moduledoc """
  Run history store (Rust metrics/mod.rs + sql.rs, #67): insert-once runner and job rows (MX1, MX2),
  first finish wins, timings merge after finish, queued_at clamp and attempts (MX4), job outcome
  precedence (MX5) and retention (MX7). Ported from metrics/mod.rs tests.
  """
  use KuiperCoordinator.DataCase, async: true

  alias KuiperCoordinator.RunHistory

  defp new_runner(name, job_key \\ nil, labels \\ []) do
    %{
      runner_name: name,
      agent_id: "agent-1",
      hostname: "mac-1",
      agent_type: "tart",
      forge: "github",
      job_key: job_key,
      repository: "o/r",
      workflow_name: "CI",
      job_name: "build",
      pool: nil,
      labels: labels
    }
  end

  defp seen_job(key, opts \\ []) do
    RunHistory.job_seen(%{
      job_key: key,
      repository: Keyword.get(opts, :repository),
      workflow_name: nil,
      job_name: nil,
      labels: Keyword.get(opts, :labels, []),
      queued_at: Keyword.get(opts, :queued_at)
    })
  end

  defp job(key), do: Enum.find(RunHistory.jobs_since(0), &(&1.job_key == key))
  defp run(name), do: Enum.find(RunHistory.runs_since(0), &(&1.runner_name == name))

  test "runner lifecycle: first finish wins, timings merge after finish (MX2)" do
    labels = ["self-hosted", "macOS"]
    :ok = RunHistory.runner_requested(new_runner("r1", "github:42", labels))
    # a second request for the same runner is ignored
    :ok = RunHistory.runner_requested(%{new_runner("r1") | hostname: "other"})
    :ok = RunHistory.runner_acked("r1")
    :ok = RunHistory.runner_started("r1")
    :ok = RunHistory.runner_job_started("r1", DateTime.utc_now())

    :ok =
      RunHistory.runner_timings("r1", %{
        steps_ms: %{"create" => 1_000, "boot" => 20_000, "install" => 0, "setup" => 3_000},
        image: "macos-15",
        install_source: "hit",
        failed_phase: ""
      })

    :ok = RunHistory.runner_finished("r1", :failed, "boom")
    :ok = RunHistory.runner_finished("r1", :lost, "")

    :ok =
      RunHistory.runner_timings("r1", %{
        steps_ms: %{"setup" => 3_100, "destroy" => 500},
        failed_phase: "job",
        image: "",
        install_source: ""
      })

    r = run("r1")
    assert r.hostname == "mac-1"
    assert r.job_key == "github:42"
    assert r.labels == labels
    assert r.acked_at && r.started_at && r.job_started_at && r.finished_at
    assert r.outcome == "failed"
    assert r.error == "boom"
    assert r.image == "macos-15"
    assert r.failed_phase == "job"
    assert r.install_source == "hit"

    assert r.steps == %{
             "boot" => 20_000,
             "create" => 1_000,
             "destroy" => 500,
             "install" => 0,
             "setup" => 3_100
           }
  end

  test "ack/start times are set once" do
    :ok = RunHistory.runner_requested(new_runner("r2"))
    :ok = RunHistory.runner_acked("r2")
    first = run("r2").acked_at
    Process.sleep(5)
    :ok = RunHistory.runner_acked("r2")
    assert run("r2").acked_at == first
  end

  test "an empty error is stored as nil" do
    :ok = RunHistory.runner_requested(new_runner("r3"))
    :ok = RunHistory.runner_finished("r3", :completed, "")
    assert %{outcome: "completed", error: nil} = run("r3")
  end

  test "job outcomes: ran wins over gone, abandoned only fills blanks, attempts count (MX4, MX5)" do
    queued = DateTime.add(DateTime.utc_now(), -120, :second)
    :ok = seen_job("github:7", repository: "o/r", labels: ["linux"], queued_at: queued)
    :ok = RunHistory.job_runner_created("github:7")
    :ok = RunHistory.job_runner_created("github:7")
    :ok = RunHistory.job_ended("github:7", :gone)
    :ok = RunHistory.job_ended("github:7", :ran)
    :ok = RunHistory.job_ended("github:7", :abandoned)

    j = job("github:7")
    assert j.forge == "github"
    assert j.queued_at == DateTime.to_unix(queued, :millisecond)
    assert j.attempts == 2
    assert j.first_runner_at
    assert j.outcome == "ran"
    assert j.labels == ["linux"]
  end

  test "a forge clock ahead of ours doesn't make the wait negative (MX4)" do
    :ok = seen_job("github:70", queued_at: DateTime.add(DateTime.utc_now(), 600, :second))
    j = job("github:70")
    assert j.queued_at == j.seen_at
  end

  test "a cancel replaces a started job (MX5)" do
    :ok = seen_job("github:9")
    :ok = RunHistory.job_started("github:9", "r1", DateTime.utc_now())
    :ok = RunHistory.job_completed("github:9", "cancelled", DateTime.utc_now())
    :ok = RunHistory.job_ended("github:9", :ran)

    j = job("github:9")
    assert j.outcome == "cancelled"
    assert j.runner_name == "r1"
    assert j.started_at
  end

  test "a late start keeps the cancel (MX5)" do
    :ok = seen_job("github:10")
    :ok = RunHistory.job_completed("github:10", "cancelled", DateTime.utc_now())
    :ok = RunHistory.job_started("github:10", "r1", DateTime.utc_now())

    j = job("github:10")
    assert j.outcome == "cancelled"
    assert j.started_at
  end

  test "a failed job still counts as ran" do
    :ok = seen_job("github:11")
    :ok = RunHistory.job_started("github:11", "r1", DateTime.utc_now())
    :ok = RunHistory.job_completed("github:11", "failure", DateTime.utc_now())
    assert %{outcome: "ran", conclusion: "failure"} = job("github:11")
  end

  test "a cancelled job that never started (MX5)" do
    :ok = seen_job("github:8")
    :ok = RunHistory.job_completed("github:8", "cancelled", DateTime.utc_now())
    :ok = RunHistory.job_ended("github:8", :ran)

    j = job("github:8")
    assert j.queued_at == j.seen_at
    assert j.conclusion == "cancelled"
    assert j.outcome == "cancelled"
  end

  test "the first job_seen wins" do
    :ok = seen_job("forgejo:3", repository: "a/b")
    :ok = seen_job("forgejo:3", repository: "c/d")
    assert %{forge: "forgejo", repository: "a/b"} = job("forgejo:3")
  end

  test "prune drops rows older than the retention (MX7)" do
    :ok = RunHistory.runner_requested(new_runner("p1"))
    :ok = seen_job("github:99")
    assert RunHistory.prune(86_400_000) == 0
    assert RunHistory.prune(-1_000) == 2
    assert RunHistory.runs_since(0) == []
    assert RunHistory.jobs_since(0) == []
  end

  test "writes never raise (best effort)" do
    assert :ok = RunHistory.runner_acked("nobody")
    assert :ok = RunHistory.job_ended("github:none", :gone)
    assert :ok = RunHistory.runner_requested(%{runner_name: "bad"})
  end

  test "prune_expired uses the configured retention (MX7)" do
    old = System.system_time(:millisecond) - 91 * 86_400_000

    RunHistory.insert_run!(%{
      runner_name: "old",
      agent_id: "a",
      hostname: "h",
      agent_type: "tart",
      requested_at: old
    })

    :ok = RunHistory.runner_requested(new_runner("fresh"))
    assert RunHistory.retention_days() == 90
    assert RunHistory.prune_expired() == 1
    assert [%{runner_name: "fresh"}] = RunHistory.runs_since(0)
  end
end
