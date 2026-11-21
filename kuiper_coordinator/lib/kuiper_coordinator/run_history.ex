defmodule KuiperCoordinator.RunHistory do
  @moduledoc """
  Run history for the metrics page (Rust metrics/mod.rs + sql.rs, #67): when each runner and job
  reached each step, and how it ended. Times are unix millis.

  Writes are best effort, like Rust: a failed write is logged and never raises, so it can't hold
  up provisioning. Every write returns `:ok`.

  Semantics carried over (parity MX2-MX5, MX7):
    * a runner or job row is inserted once; later requests for the same name/key are ignored
    * acked/started/job_started times are set only while blank
    * only the first finish counts (an agent can send Failed then Destroyed)
    * timings apply after the finish too, steps merge into what's there
    * queued_at is the forge's created time, never later than seen_at
    * a cancel replaces ran/gone/abandoned and a late start keeps it; ran beats any non-cancel;
      abandoned and gone only fill a blank
  """
  import Ecto.Query
  require Logger

  alias KuiperCoordinator.Repo
  alias KuiperCoordinator.RunHistory.{JobRun, RunnerRun}

  @run_outcomes ~w(completed failed lost rejected command_failed idled drained destroyed)a
  @job_outcomes ~w(ran abandoned gone)a

  @doc "Runner outcomes, as atoms."
  def run_outcomes, do: @run_outcomes

  defp now_ms, do: System.system_time(:millisecond)
  defp ms(%DateTime{} = at), do: DateTime.to_unix(at, :millisecond)
  defp blank_nil(s) when s in [nil, ""], do: nil
  defp blank_nil(s), do: to_string(s)

  defp safely(what, key, fun) do
    fun.()
    :ok
  rescue
    e ->
      Logger.warning(
        "Failed to record #{what} for #{inspect(key)} in the run history: #{Exception.message(e)}"
      )

      :ok
  end

  defp exec(sql, params), do: Repo.query!(sql, params)

  ## runners

  @doc """
  A CreateRunner is about to be sent to `agent_id`. The agent's hostname and type are copied in
  (the agent row goes away once it's revoked).
  """
  def runner_requested_on(agent_id, run) do
    {hostname, type} =
      case safe_agent(agent_id) do
        %{hostname: h, agent_type: t} -> {h, t}
        nil -> {agent_id, "unknown"}
      end

    runner_requested(Map.merge(run, %{agent_id: agent_id, hostname: hostname, agent_type: type}))
  end

  defp safe_agent(agent_id) do
    KuiperCoordinator.Agents.get_agent(agent_id)
  rescue
    _ -> nil
  end

  @doc "A CreateRunner is about to be sent."
  def runner_requested(%{runner_name: name} = run) do
    safely("runner request", name, fn ->
      row = %{
        runner_name: name,
        agent_id: Map.fetch!(run, :agent_id),
        hostname: Map.fetch!(run, :hostname),
        agent_type: Map.fetch!(run, :agent_type),
        forge: run[:forge],
        job_key: run[:job_key] && to_string(run[:job_key]),
        repository: run[:repository],
        workflow_name: run[:workflow_name],
        job_name: run[:job_name],
        pool: run[:pool],
        labels: run[:labels] || [],
        requested_at: now_ms(),
        steps: %{}
      }

      Repo.insert_all(RunnerRun, [row], on_conflict: :nothing, conflict_target: :runner_name)
    end)
  end

  def runner_acked(name), do: set_once(:acked_at, "runner ack", name, now_ms())
  def runner_started(name), do: set_once(:started_at, "runner start", name, now_ms())

  @doc "The forge says the runner took its job."
  def runner_job_started(name, %DateTime{} = at),
    do: set_once(:job_started_at, "runner job start", name, ms(at))

  defp set_once(field, what, name, at) do
    safely(what, name, fn ->
      Repo.update_all(
        from(r in RunnerRun, where: r.runner_name == ^name and is_nil(field(r, ^field))),
        set: [{field, at}]
      )
    end)
  end

  @doc "How a runner ended. Only the first call counts."
  def runner_finished(name, outcome, error) when outcome in @run_outcomes do
    safely("runner finish", name, fn ->
      Repo.update_all(
        from(r in RunnerRun, where: r.runner_name == ^name and is_nil(r.finished_at)),
        set: [finished_at: now_ms(), outcome: Atom.to_string(outcome), error: blank_nil(error)]
      )
    end)
  end

  @doc """
  The agent's RunnerTimings (a proto struct or a map with steps_ms, image, failed_phase,
  install_source). Empty strings keep what's there; steps merge.
  """
  def runner_timings(name, timings) do
    safely("runner timings", name, fn ->
      steps =
        Map.new(Map.get(timings, :steps_ms) || %{}, fn {k, v} ->
          {to_string(k), min(v, 9_223_372_036_854_775_807)}
        end)

      exec(
        """
        UPDATE runner_runs SET
          image = COALESCE($1, image),
          failed_phase = COALESCE($2, failed_phase),
          install_source = COALESCE($3, install_source),
          steps = COALESCE(steps, '{}'::jsonb) || $4::jsonb
        WHERE runner_name = $5
        """,
        [
          blank_nil(Map.get(timings, :image)),
          blank_nil(Map.get(timings, :failed_phase)),
          blank_nil(Map.get(timings, :install_source)),
          steps,
          name
        ]
      )
    end)
  end

  ## jobs

  @doc "A job was added to the pending queue. `queued_at` is the forge's created time, if any."
  def job_seen(%{job_key: key} = job) do
    key = to_string(key)

    safely("job", key, fn ->
      seen = now_ms()
      queued = if job[:queued_at], do: min(ms(job[:queued_at]), seen), else: seen
      [forge | _] = String.split(key, ":", parts: 2)

      row = %{
        job_key: key,
        forge: forge,
        repository: job[:repository],
        workflow_name: job[:workflow_name],
        job_name: job[:job_name],
        labels: job[:labels] || [],
        queued_at: queued,
        seen_at: seen,
        attempts: 0
      }

      Repo.insert_all(JobRun, [row], on_conflict: :nothing, conflict_target: :job_key)
    end)
  end

  @doc "A runner was created for the job: attempts + 1, first_runner_at once."
  def job_runner_created(key) do
    key = to_string(key)

    safely("job runner", key, fn ->
      exec(
        "UPDATE job_runs SET attempts = attempts + 1, first_runner_at = COALESCE(first_runner_at, $1) WHERE job_key = $2",
        [now_ms(), key]
      )
    end)
  end

  @doc "The forge says `runner_name` took the job. A cancel already recorded stays."
  def job_started(key, runner_name, %DateTime{} = at) do
    key = to_string(key)

    safely("job start", key, fn ->
      exec(
        """
        UPDATE job_runs SET started_at = COALESCE(started_at, $1),
          runner_name = COALESCE(runner_name, $2),
          outcome = CASE WHEN outcome = 'cancelled' THEN outcome ELSE 'ran' END
        WHERE job_key = $3
        """,
        [ms(at), runner_name, key]
      )
    end)
  end

  @doc "The forge says the job finished with `conclusion`. A cancel replaces any outcome."
  def job_completed(key, conclusion, %DateTime{} = at) do
    key = to_string(key)
    outcome = if conclusion == "cancelled", do: "cancelled", else: "ran"

    safely("job completion", key, fn ->
      exec(
        """
        UPDATE job_runs SET completed_at = COALESCE(completed_at, $1),
          conclusion = COALESCE($2, conclusion),
          outcome = CASE WHEN $3::text = 'cancelled' THEN 'cancelled' ELSE COALESCE(outcome, $3::text) END
        WHERE job_key = $4
        """,
        [ms(at), conclusion, outcome, key]
      )
    end)
  end

  @doc "The job left the pending queue: ran beats anything but a cancel; the rest fill a blank."
  def job_ended(key, outcome) when outcome in @job_outcomes do
    key = to_string(key)
    outcome = Atom.to_string(outcome)

    safely("job outcome", key, fn ->
      exec(
        """
        UPDATE job_runs SET outcome = CASE
          WHEN $1::text = 'ran' AND COALESCE(outcome, '') <> 'cancelled' THEN 'ran'
          ELSE COALESCE(outcome, $1::text)
        END
        WHERE job_key = $2
        """,
        [outcome, key]
      )
    end)
  end

  ## reads and retention

  @doc "Runner rows requested at or after `since_ms`, oldest first."
  def runs_since(since_ms),
    do:
      Repo.all(from(r in RunnerRun, where: r.requested_at >= ^since_ms, order_by: r.requested_at))

  @doc "Job rows seen at or after `since_ms`, oldest first."
  def jobs_since(since_ms),
    do: Repo.all(from(j in JobRun, where: j.seen_at >= ^since_ms, order_by: j.seen_at))

  @doc "Deletes runner rows requested and job rows seen more than `retention_ms` ago; returns the count."
  def prune(retention_ms) do
    cutoff = now_ms() - retention_ms
    {runs, _} = Repo.delete_all(from(r in RunnerRun, where: r.requested_at < ^cutoff))
    {jobs, _} = Repo.delete_all(from(j in JobRun, where: j.seen_at < ^cutoff))
    runs + jobs
  end

  @doc "Retention in days (`config :kuiper_coordinator, :metrics, retention_days:`, default 90)."
  def retention_days,
    do: Application.get_env(:kuiper_coordinator, :metrics, []) |> Keyword.get(:retention_days, 90)

  @doc "Prunes rows past `retention_days/0` (the hourly janitor, Rust main.rs:523-539)."
  def prune_expired, do: prune(retention_days() * 86_400_000)

  @doc "Inserts whole rows as given, for seeding example history."
  def insert_run!(attrs), do: Repo.insert!(struct!(RunnerRun, attrs))
  def insert_job!(attrs), do: Repo.insert!(struct!(JobRun, attrs))
end
