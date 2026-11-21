defmodule KuiperCoordinator.Jobs.PendingTest do
  use KuiperCoordinator.DataCase, async: true

  alias KuiperCoordinator.Jobs

  defp attrs(id, extra \\ %{}) do
    Map.merge(
      %{
        job_key: "github:#{id}",
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

    assert [%{job_key: "github:1", job_name: nil, runner_scope: {:org, "acme"}}] =
             Jobs.list_pending()
  end

  test "pending jobs come back oldest first (J2)" do
    for id <- [3, 1, 2], do: {:ok, :inserted} = Jobs.add_pending(attrs(id))
    assert Enum.map(Jobs.list_pending(), & &1.job_key) == ["github:3", "github:1", "github:2"]
  end

  test "a retry records each failed agent once (port of pending_jobs.rs:454, #30)" do
    {:ok, :inserted} = Jobs.add_pending(attrs(1))
    assert {:ok, 1} = Jobs.record_retry("github:1", "agent-a")
    assert {:ok, 2} = Jobs.record_retry("github:1", "agent-a")
    assert {:ok, 3} = Jobs.record_retry("github:1", "agent-b")
    assert {:ok, 4} = Jobs.record_retry("github:1", nil)
    assert %{retry_count: 4, failed_agents: ["agent-a", "agent-b"]} = Jobs.get_pending("github:1")
    assert {:error, :not_found} = Jobs.record_retry("github:404", "agent-a")
  end

  test "remove" do
    {:ok, :inserted} = Jobs.add_pending(attrs(1))
    assert :ok = Jobs.remove_pending("github:1")
    assert Jobs.get_pending("github:1") == nil
  end
end
