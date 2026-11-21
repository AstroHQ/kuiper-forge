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
end
