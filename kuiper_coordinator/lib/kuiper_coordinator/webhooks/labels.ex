defmodule KuiperCoordinator.Webhooks.Labels do
  @moduledoc """
  Which queued jobs we take, and where their runners register (parity W5, W6). Shared by the
  webhook and the polling/recovery scan so both accept the same jobs.

  A label mapping is `%{labels: [String.t()], runner_scope: scope | nil, runner_group:
  String.t() | nil, forge: String.t() | nil}`.
  """

  @doc "Jobs must carry these unless configured otherwise (#4, e8b974a)."
  def default_required, do: ["self-hosted"]

  @doc "True when every required label is on the job (case-insensitive)."
  def has_required?(job_labels, required) do
    job = MapSet.new(job_labels, &String.downcase/1)
    Enum.all?(required, &MapSet.member?(job, String.downcase(&1)))
  end

  @doc "The first mapping for this forge (or any forge) whose labels are all on the job."
  def match(forge, job_labels, mappings) do
    Enum.find(mappings, fn m ->
      m[:forge] in [nil, forge] and has_required?(job_labels, m.labels)
    end)
  end

  @doc """
  `{:ok, scope, runner_group}` for a job, or `:ignore` when mappings exist and none match.
  With no mappings every job gets `default_scope`.
  """
  def resolve_scope(_forge, _job_labels, [], default_scope), do: {:ok, default_scope, nil}

  def resolve_scope(forge, job_labels, mappings, default_scope) do
    case match(forge, job_labels, mappings) do
      nil -> :ignore
      m -> {:ok, m.runner_scope || default_scope, m.runner_group}
    end
  end
end
