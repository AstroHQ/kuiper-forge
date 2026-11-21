defmodule KuiperCoordinator.Runners do
  @moduledoc """
  Runner rows (Rust runner_state.rs). The DB is the source of truth for how many runners a pool
  has, not an in-memory counter (parity P3, ENG-4829).
  """
  import Ecto.Query

  alias KuiperCoordinator.Repo
  alias KuiperCoordinator.Runners.Runner

  @fields ~w(runner_name agent_id vm_name runner_scope forge job_key job_name repository
             workflow_name pool labels)a

  @doc """
  Writes the row for a runner about to be created (before CreateRunner is sent, F3).
  A database error comes back as `{:error, exception}` rather than raising, so callers can give
  the reserved slot back (F8; Rust add_runner logs and carries on).
  """
  @spec add_runner(map()) :: {:ok, Runner.t()} | {:error, term()}
  def add_runner(attrs) do
    %Runner{}
    |> Ecto.Changeset.cast(Map.new(attrs), @fields)
    |> Ecto.Changeset.validate_required(~w(runner_name agent_id vm_name runner_scope forge)a)
    |> Repo.insert()
  rescue
    e in [DBConnection.ConnectionError, Postgrex.Error] -> {:error, e}
  end

  def get_runner(name), do: Repo.get(Runner, name)

  def list_runners, do: Repo.all(from r in Runner, order_by: r.inserted_at)

  @doc "Runner names bound to a job (Rust runners_for_job)."
  @spec for_job(String.t()) :: [String.t()]
  def for_job(job_key),
    do: Repo.all(from r in Runner, where: r.job_key == ^job_key, select: r.runner_name)

  @spec remove_runner(String.t()) :: :ok
  @doc "Names of the runners recorded on `agent_id`."
  def names_for_agent(agent_id),
    do: Repo.all(from r in Runner, where: r.agent_id == ^agent_id, select: r.runner_name)

  def remove_runner(name) do
    Repo.delete_all(from r in Runner, where: r.runner_name == ^name)
    :ok
  end

  @doc "Runners of an explicit pool: that agent's runners tagged with the pool key."
  @spec count_for_pool(String.t(), String.t()) :: non_neg_integer()
  def count_for_pool(agent_id, pool_key) do
    Repo.aggregate(
      from(r in Runner, where: r.agent_id == ^agent_id and r.pool == ^pool_key),
      :count
    )
  end

  @doc "All runners on the given agents (a legacy pool's members)."
  @spec count_for_agents([String.t()]) :: non_neg_integer()
  def count_for_agents([]), do: 0

  def count_for_agents(agent_ids) do
    Repo.aggregate(from(r in Runner, where: r.agent_id in ^agent_ids), :count)
  end
end
