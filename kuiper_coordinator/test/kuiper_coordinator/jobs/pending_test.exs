defmodule KuiperCoordinator.Jobs.PendingTest do
  use KuiperCoordinator.DataCase, async: true

  # job_key is the primary key, and this file runs alongside other async tests
  # (fleet/recovery_test.exs) that insert "github:N" keys. Overlapping keys
  # inserted in different orders by two sandboxed transactions deadlock in
  # Postgres, so this file keeps its own key space.

  alias KuiperCoordinator.Jobs

  defp attrs(id, extra \\ %{}) do
    Map.merge(
      %{
        job_key: "pending-test:#{id}",
        forge: "github",
        job_labels: ["self-hosted", "linux"],
        agent_labels: ["self-hosted", "linux"],
        runner_scope: {:org, "acme"},
        repository: "acme/app"
      },
      extra
    )
  end

  test "a duplicate queued delivery makes one pending job (J1)" do
    assert {:ok, :inserted} = Jobs.add_pending(attrs(1))
    assert {:ok, :duplicate} = Jobs.add_pending(attrs(1, %{job_name: "again"}))

    assert [%{job_key: "pending-test:1", job_name: nil, runner_scope: {:org, "acme"}}] =
             Jobs.list_pending()
  end

  test "pending jobs come back oldest first (J2)" do
    for id <- [3, 1, 2], do: {:ok, :inserted} = Jobs.add_pending(attrs(id))

    assert Enum.map(Jobs.list_pending(), & &1.job_key) == [
             "pending-test:3",
             "pending-test:1",
             "pending-test:2"
           ]
  end

  test "a retry records each failed agent once (port of pending_jobs.rs:454, #30)" do
    {:ok, :inserted} = Jobs.add_pending(attrs(1))
    assert {:ok, 1} = Jobs.record_retry("pending-test:1", "agent-a")
    assert {:ok, 2} = Jobs.record_retry("pending-test:1", "agent-a")
    assert {:ok, 3} = Jobs.record_retry("pending-test:1", "agent-b")
    assert {:ok, 4} = Jobs.record_retry("pending-test:1", nil)

    assert %{retry_count: 4, failed_agents: ["agent-a", "agent-b"]} =
             Jobs.get_pending("pending-test:1")

    assert {:error, :not_found} = Jobs.record_retry("pending-test:404", "agent-a")
  end

  test "remove" do
    {:ok, :inserted} = Jobs.add_pending(attrs(1))
    assert :ok = Jobs.remove_pending("pending-test:1")
    assert Jobs.get_pending("pending-test:1") == nil
  end
end
