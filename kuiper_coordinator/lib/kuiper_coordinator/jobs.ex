defmodule KuiperCoordinator.Jobs do
  @moduledoc "Pending forge jobs (parity J1-J4)."
  import Ecto.Query
  require Logger

  alias KuiperCoordinator.Repo
  alias KuiperCoordinator.Jobs.PendingJob

  @doc "Failures (retries) after which a job is abandoned (Rust MAX_RETRIES, F7)."
  def max_retries, do: 3

  @fields ~w(job_key forge job_labels agent_labels runner_scope runner_group repository job_name workflow_name queued_at)a

  @doc """
  Persists a queued job; a redelivery of the same job is a no-op (`{:ok, :duplicate}`).
  `{:error, reason}` when the database can't take it, so the webhook answers 503 and the
  forge redelivers (parity J1).
  """
  @spec add_pending(map()) :: {:ok, :inserted | :duplicate} | {:error, term()}
  def add_pending(attrs) do
    changeset =
      %PendingJob{}
      |> Ecto.Changeset.cast(attrs, @fields)
      |> Ecto.Changeset.validate_required([:job_key, :forge, :runner_scope])

    with {:ok, job} <- Ecto.Changeset.apply_action(changeset, :insert) do
      now = DateTime.utc_now()

      row =
        job
        |> Map.take(@fields ++ [:retry_count, :failed_agents])
        |> Map.merge(%{inserted_at: now, updated_at: now})

      case Repo.insert_all(PendingJob, [row], on_conflict: :nothing, conflict_target: :job_key) do
        {1, _} -> {:ok, :inserted}
        {0, _} -> {:ok, :duplicate}
      end
    end
  rescue
    e in [DBConnection.ConnectionError, DBConnection.OwnershipError, Postgrex.Error] ->
      Logger.warning("Failed to persist pending job: #{Exception.message(e)}")
      {:error, e}
  catch
    # a dead pool or connection owner exits rather than raising
    :exit, reason ->
      Logger.warning("Failed to persist pending job: #{inspect(reason)}")
      {:error, {:exit, reason}}
  end

  @doc "All pending jobs, oldest first."
  def list_pending do
    Repo.all(from j in PendingJob, order_by: [asc: j.inserted_at, asc: j.job_key])
  end

  def get_pending(job_key), do: Repo.get(PendingJob, job_key)

  def remove_pending(job_key) do
    Repo.delete_all(from j in PendingJob, where: j.job_key == ^job_key)
    :ok
  end

  @doc """
  Bumps a job's retry count and adds `agent_id` (if any) to its avoid-list once
  (parity J4, #30). Returns the new retry count.
  """
  @spec record_retry(String.t(), String.t() | nil) :: {:ok, pos_integer()} | {:error, :not_found}
  def record_retry(job_key, agent_id) do
    base = from j in PendingJob, as: :j, where: j.job_key == ^job_key, select: j.retry_count

    query =
      case agent_id do
        nil ->
          update(base, inc: [retry_count: 1])

        agent ->
          update(base,
            inc: [retry_count: 1],
            set: [
              failed_agents:
                fragment(
                  "CASE WHEN ? = ANY(?) THEN ? ELSE array_append(?, ?) END",
                  ^agent,
                  field(as(:j), :failed_agents),
                  field(as(:j), :failed_agents),
                  field(as(:j), :failed_agents),
                  ^agent
                )
            ]
          )
      end

    case Repo.update_all(query, []) do
      {1, [count]} -> {:ok, count}
      {0, _} -> {:error, :not_found}
    end
  end
end
