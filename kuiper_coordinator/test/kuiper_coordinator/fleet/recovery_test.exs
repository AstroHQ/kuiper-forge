defmodule KuiperCoordinator.Fleet.RecoveryTest do
  use KuiperCoordinator.DataCase, async: true
  import Mox

  alias KuiperCoordinator.{ForgeMock, Jobs, Repo, Runners}
  alias KuiperCoordinator.Fleet.Recovery
  alias KuiperCoordinator.Forge.QueuedJob
  alias KuiperCoordinator.Jobs.PendingJob

  setup :verify_on_exit!

  @scope {:org, "astro"}
  @ctx %{forges: %{"github" => {ForgeMock, :gh}}, webhook: []}

  defp pending(id, attrs \\ %{}) do
    {:ok, :inserted} =
      Jobs.add_pending(
        Map.merge(
          %{
            job_key: "github:#{id}",
            forge: "github",
            runner_scope: @scope,
            repository: "astro/app"
          },
          attrs
        )
      )
  end

  defp queued(id, labels, extra \\ []),
    do: struct!(QueuedJob, [id: to_string(id), labels: labels, runner_scope: @scope] ++ extra)

  defp keys, do: Jobs.list_pending() |> Enum.map(& &1.job_key) |> Enum.sort()

  describe "verify_pending (Rust verify_pending_jobs)" do
    test "keeps queued, waiting and unreachable jobs; drops finished and gone ones" do
      for id <- 1..5, do: pending(id)

      statuses = %{
        "1" => {:ok, "queued"},
        "2" => {:ok, "waiting"},
        "3" => {:ok, "in_progress"},
        "4" => {:ok, nil},
        "5" => {:error, :timeout}
      }

      expect(ForgeMock, :job_status, 5, fn :gh, "astro/app", id -> Map.fetch!(statuses, id) end)

      assert Recovery.verify_pending(@ctx) == 2
      assert keys() == ["github:1", "github:2", "github:5"]
    end

    test "a job on an unknown forge is dropped" do
      pending(1, %{job_key: "forgejo:1", forge: "forgejo"})
      assert Recovery.verify_pending(@ctx) == 1
      assert keys() == []
    end

    test "a job without a repository is dropped only after 24h" do
      pending(1, %{repository: nil})
      pending(2, %{repository: nil})
      old = DateTime.add(DateTime.utc_now(), -25 * 3600)

      Repo.update_all(from(j in PendingJob, where: j.job_key == "github:1"),
        set: [inserted_at: old]
      )

      assert Recovery.verify_pending(@ctx) == 1
      assert keys() == ["github:2"]
    end
  end

  describe "one forge at a time (each forge has its own mode and interval)" do
    @two %{forges: %{"github" => {ForgeMock, :gh}, "other" => {ForgeMock, :other}}, webhook: []}

    test "a scan of some forges lists and checks only those, plus jobs of forges that are gone" do
      pending(1)
      pending(2, %{job_key: "other:2", forge: "other"})
      pending(3, %{job_key: "gone:3", forge: "gone"})

      # nothing is asked of :other: Mox fails the test if it is
      expect(ForgeMock, :job_status, fn :gh, "astro/app", "1" -> {:ok, nil} end)
      expect(ForgeMock, :list_queued_jobs, fn :gh -> {:ok, [queued(9, ["self-hosted"])]} end)

      assert Recovery.scan(@two, ["github"]) == %{dropped: 2, recovered: 1}
      assert keys() == ["github:9", "other:2"]
    end

    test "a poll of some forges leaves the other forges' pending jobs alone" do
      pending(1)
      pending(2, %{job_key: "other:2", forge: "other"})

      expect(ForgeMock, :list_queued_jobs, fn :other -> {:ok, []} end)
      expect(ForgeMock, :job_status, fn :other, "astro/app", "2" -> {:ok, "completed"} end)

      assert Recovery.poll(@two, ["other"]) == %{dropped: 1, recovered: 0}
      assert keys() == ["github:1"]
    end
  end

  describe "recover_queued (Rust recover_queued_jobs)" do
    test "adds queued jobs the webhook would take, with their scope and labels" do
      created = ~U[2026-11-21 10:00:00.000000Z]

      expect(ForgeMock, :list_queued_jobs, fn :gh ->
        {:ok,
         [
           queued(10, ["self-hosted", "macos"], repository: "astro/app", created_at: created),
           # no self-hosted label: the webhook ignores it too
           queued(11, ["ubuntu-latest"])
         ]}
      end)

      assert Recovery.recover_queued(@ctx) == 1
      assert [%PendingJob{} = job] = Jobs.list_pending()
      assert job.job_key == "github:10"
      assert job.forge == "github"
      assert job.job_labels == ["self-hosted", "macos"]
      assert job.agent_labels == ["self-hosted", "macos"]
      assert job.runner_scope == @scope
      assert job.repository == "astro/app"
      assert job.queued_at == created
      assert job.runner_group == nil
    end

    test "label mappings pick scope and group, and a job no mapping takes is skipped" do
      mappings = [
        %{labels: ["gpu"], runner_scope: {:repo, "astro", "gpu"}, runner_group: "gpu", forge: nil}
      ]

      expect(ForgeMock, :list_queued_jobs, fn :gh ->
        {:ok, [queued(1, ["self-hosted", "gpu"]), queued(2, ["self-hosted", "macos"])]}
      end)

      assert Recovery.recover_queued(%{@ctx | webhook: [label_mappings: mappings]}) == 1
      assert [job] = Jobs.list_pending()
      assert job.job_key == "github:1"
      assert job.runner_scope == {:repo, "astro", "gpu"}
      assert job.runner_group == "gpu"
    end

    test "configured required labels replace the default" do
      expect(ForgeMock, :list_queued_jobs, fn :gh ->
        {:ok, [queued(1, ["self-hosted"]), queued(2, ["kuiper", "self-hosted"])]}
      end)

      assert Recovery.recover_queued(%{@ctx | webhook: [required_labels: ["Kuiper"]]}) == 1
      assert keys() == ["github:2"]
    end

    test "skips jobs already pending or already given a runner" do
      pending(1)

      {:ok, _} =
        Runners.add_runner(%{
          runner_name: "mac1-1",
          agent_id: "a1",
          vm_name: "mac1-1",
          runner_scope: @scope,
          forge: "github",
          job_key: "github:2"
        })

      expect(ForgeMock, :list_queued_jobs, fn :gh ->
        {:ok, [queued(1, ["self-hosted"]), queued(2, ["self-hosted"])]}
      end)

      assert Recovery.recover_queued(@ctx) == 0
      assert keys() == ["github:1"]
    end

    test "a forge that can't be listed is skipped; the others are still scanned" do
      ctx = %{@ctx | forges: %{"github" => {ForgeMock, :gh}, "other" => {ForgeMock, :other}}}

      expect(ForgeMock, :list_queued_jobs, 2, fn
        :gh -> {:error, :rate_limited}
        :other -> {:ok, [queued(7, ["self-hosted"])]}
      end)

      assert Recovery.recover_queued(ctx) == 1
      assert keys() == ["other:7"]
    end
  end

  test "scan verifies pending jobs before recovering missed ones" do
    pending(1)
    test = self()

    expect(ForgeMock, :job_status, fn :gh, "astro/app", "1" ->
      send(test, :verified)
      {:ok, "completed"}
    end)

    expect(ForgeMock, :list_queued_jobs, fn :gh ->
      assert_received :verified
      {:ok, [queued(2, ["self-hosted"])]}
    end)

    assert Recovery.scan(@ctx) == %{dropped: 1, recovered: 1}
    assert keys() == ["github:2"]
  end

  describe "poll (polling mode, Rust scan_forges with Polling)" do
    test "adds new jobs, then checks only the pending jobs the poll no longer listed" do
      # 1 is still listed, 2 isn't (finished), 3 isn't but the forge says it's still queued
      pending(1)
      pending(2)
      pending(3)
      test = self()

      expect(ForgeMock, :list_queued_jobs, fn :gh ->
        {:ok, [queued(1, ["self-hosted"]), queued(4, ["self-hosted"])]}
      end)

      expect(ForgeMock, :job_status, 2, fn :gh, "astro/app", id ->
        send(test, {:checked, id})
        if id == "2", do: {:ok, "completed"}, else: {:ok, "queued"}
      end)

      assert Recovery.poll(@ctx) == %{dropped: 1, recovered: 1}
      assert keys() == ["github:1", "github:3", "github:4"]
      assert_received {:checked, "2"}
      assert_received {:checked, "3"}
      refute_received {:checked, "1"}
    end

    test "a forge that couldn't be listed drops nothing" do
      pending(1)
      ctx = %{@ctx | forges: %{"github" => {ForgeMock, :gh}, "fj" => {ForgeMock, :fj}}}

      {:ok, :inserted} =
        Jobs.add_pending(%{
          job_key: "fj:h-1",
          forge: "fj",
          runner_scope: @scope,
          repository: "astro/app"
        })

      expect(ForgeMock, :list_queued_jobs, 2, fn
        :gh -> {:error, :rate_limited}
        :fj -> {:ok, [queued("h-1", ["self-hosted"])]}
      end)

      # no job_status expectation: Mox fails the test if either job is checked
      assert Recovery.poll(ctx) == %{dropped: 0, recovered: 0}
      assert keys() == ["fj:h-1", "github:1"]
    end

    test "a job the poll missed but the forge can't confirm is kept" do
      pending(1)
      expect(ForgeMock, :list_queued_jobs, fn :gh -> {:ok, []} end)
      expect(ForgeMock, :job_status, fn :gh, "astro/app", "1" -> {:error, :timeout} end)
      assert Recovery.poll(@ctx) == %{dropped: 0, recovered: 0}
      assert keys() == ["github:1"]
    end

    test "dropped_pending keeps queue order and only counts listed forges" do
      jobs =
        for k <- ["gh:1", "gh:2", "gh:3", "fj:9"],
            do: %{job_key: k, forge: hd(String.split(k, ":"))}

      scan = %{forges: MapSet.new(["gh"]), seen: MapSet.new(["gh:2", "gh:99"])}
      assert Enum.map(Recovery.dropped_pending(jobs, scan), & &1.job_key) == ["gh:1", "gh:3"]
      assert Recovery.dropped_pending(jobs, %{scan | forges: MapSet.new()}) == []
    end

    test "the poll interval is 120 s by default and never under 10 s" do
      assert Recovery.poll_interval_ms(nil) == 120_000
      assert Recovery.poll_interval_ms(30) == 30_000
      assert Recovery.poll_interval_ms(1) == 10_000
    end
  end
end
