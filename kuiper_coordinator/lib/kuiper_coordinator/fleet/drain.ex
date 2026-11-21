defmodule KuiperCoordinator.Fleet.Drain do
  @moduledoc """
  Drains connected agents that are switched off (`unavailable_at` set), so the host really
  empties for maintenance (Rust fleet.rs drain_unavailable_agents, parity F16). Runs before
  each fixed-capacity reconcile.

  Each idle pool runner on such an agent is unregistered from its forge first, which the forge
  refuses while it runs a job (`Forge.remove_idle_runner/3`), and only then is its VM destroyed.
  Busy runners finish their job and exit like any ephemeral runner, and the pool doesn't
  replace them (pool definitions skip unavailable agents). Runners made for a specific job
  (a `job_key`, the on-demand modes) are left alone. A runner that isn't registered yet is kept
  for 10 minutes after it was created (it may still be booting), then destroyed anyway.

  The state is `%{runner_name => monotonic ms the DestroyRunner was sent}`: a runner is sent
  DestroyRunner again only after `@destroy_retry_ms` without its destroyed event.
  """
  require Logger

  alias KuiperCoordinator.{AgentDispatch, Forge, Runners}
  alias KuiperCoordinator.Runners.Runner

  # Rust DRAIN_BOOT_GRACE
  @boot_grace_s 600
  # Rust DRAIN_DESTROY_RETRY
  @destroy_retry_ms 300_000

  @type t :: %{String.t() => integer()}

  @doc "What to do with a pool runner on an unavailable agent (Rust drain_step)."
  @spec step(:removed | :busy | :not_registered, age_s :: integer()) :: :keep | :destroy
  def step(:removed, _age_s), do: :destroy
  def step(:busy, _age_s), do: :keep
  def step(:not_registered, age_s) when age_s < @boot_grace_s, do: :keep
  def step(:not_registered, _age_s), do: :destroy

  @doc """
  One drain pass. `ctx` is the fleet context (`:forges`, `:forge`); `now` is monotonic ms.
  Returns the new state.
  """
  @spec run(t(), map(), integer(), DateTime.t()) :: t()
  def run(draining, ctx, now, utc_now \\ DateTime.utc_now()) do
    case AgentDispatch.unavailable_agents() do
      [] ->
        %{}

      ids ->
        unavailable = MapSet.new(ids)
        runners = Runners.list_runners()
        names = MapSet.new(runners, & &1.runner_name)
        draining = Map.filter(draining, fn {name, _} -> MapSet.member?(names, name) end)

        runners
        |> Enum.filter(&(&1.job_key == nil and MapSet.member?(unavailable, &1.agent_id)))
        |> Enum.reduce(draining, &drain_runner(&2, ctx, &1, now, utc_now))
    end
  end

  defp drain_runner(draining, ctx, %Runner{runner_name: name} = row, now, utc_now) do
    case Map.fetch(draining, name) do
      {:ok, at} when now - at < @destroy_retry_ms -> draining
      _ -> check(draining, ctx, row, now, utc_now)
    end
  end

  defp check(draining, ctx, row, now, utc_now) do
    age_s = DateTime.diff(utc_now, row.inserted_at, :second)

    case Forge.remove_idle_runner(forge(ctx, row), row.runner_scope, row.runner_name) do
      {:ok, removal} ->
        case step(removal, age_s) do
          :keep ->
            Logger.debug(
              "Keeping runner #{row.runner_name} on unavailable agent #{row.agent_id} for now " <>
                "(#{removal}, #{age_s} s old)"
            )

            draining

          :destroy ->
            destroy(draining, row, removal, age_s, now)
        end

      {:error, e} ->
        Logger.warning(
          "Couldn't check runner #{row.runner_name} to drain it, retrying next reconcile: " <>
            inspect(e)
        )

        draining
    end
  end

  defp destroy(draining, row, removal, age_s, now) do
    case AgentDispatch.dispatch_destroy(row.agent_id, row.vm_name) do
      :ok ->
        Logger.info(
          "Draining idle runner #{row.runner_name} (VM #{row.vm_name}) from unavailable agent " <>
            "#{row.agent_id} (#{removal}, #{age_s} s old)"
        )

        Map.put(draining, row.runner_name, now)

      {:error, e} ->
        Logger.warning(
          "Failed to send DestroyRunner to drain runner #{row.runner_name}: #{inspect(e)}"
        )

        draining
    end
  end

  # Rust runner_forge: the runner's own forge, else the default one
  defp forge(ctx, %Runner{forge: name}),
    do: Map.get(Map.get(ctx, :forges, %{}), name) || ctx.forge
end
