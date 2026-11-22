defmodule KuiperCoordinatorWeb.Plugs.ForgejoWebhook do
  @moduledoc """
  Forgejo 17+ `workflow_job_*` webhooks (parity W4), ported from `handle_forgejo` in
  kuiper-forge/src/webhook.rs. `KuiperCoordinatorWeb.Plugs.GitHubWebhook` hands deliveries
  here when the webhook's config has `source: :forgejo`.

  The event is `X-Forgejo-Event: workflow_job_<status>` and the signature
  `X-Forgejo-Signature: <hex>` (no `sha256=` prefix). Other events get 200 before the
  signature is checked, a bad signature 401, an unparseable payload 400.

  Jobs are keyed `<forge>:<handle>`, like the polling/recovery scan, so a job the scan also
  found is queued once. `waiting` queues the job in the configured scope that covers its repo
  (repo, else owner, else instance; none means ignored); `blocked` waits for its `waiting`
  delivery; `running` and the finished states are announced on PubSub `"fleet"` as GitHub's
  `in_progress` and `completed` are.

  Extra config (`config :kuiper_coordinator, :webhook`): `:scopes`, the Forgejo forge's
  runner scopes (`KuiperCoordinator.Forge.RunnerScope`).
  """
  import Plug.Conn
  require Logger

  alias KuiperCoordinator.{Jobs, RunHistory, Webhooks.Labels}
  alias KuiperCoordinatorWeb.Plugs.GitHubWebhook

  @prefix "workflow_job_"
  @finished ["success", "failure", "cancelled", "skipped"]

  @doc false
  def handle(conn, config) do
    event_type = header(conn, "x-forgejo-event")

    with {:event, @prefix <> status} <- {:event, event_type},
         {:ok, body, conn} <-
           GitHubWebhook.read_full_body(conn, Keyword.get(config, :max_body, 25 * 1024 * 1024)),
         {:sig, true} <-
           {:sig, valid_signature?(config[:secret], body, header(conn, "x-forgejo-signature"))},
         {:json, {:ok, event}} <- {:json, Jason.decode(body)},
         {:parse, {:ok, parsed}} <- {:parse, parse(event)} do
      send_resp(conn, handle_event(status, event_type, parsed, config), "")
    else
      {:event, other} ->
        Logger.debug("Ignoring Forgejo webhook event type: #{inspect(other)}")
        send_resp(conn, 200, "")

      {:sig, false} ->
        Logger.warning(
          "Forgejo webhook signature validation failed for #{event_type} on forge " <>
            "'#{Keyword.get(config, :forge, "forgejo")}'"
        )

        send_resp(conn, 401, "")

      {:json, _} ->
        Logger.warning("Failed to parse Forgejo #{event_type} payload: bad JSON")
        send_resp(conn, 400, "")

      {:parse, {:error, why}} ->
        Logger.warning("Failed to parse Forgejo #{event_type} payload: #{why}")
        send_resp(conn, 400, "")

      {:error, :too_large} ->
        send_resp(conn, 413, "")

      {:error, reason} ->
        Logger.warning("Failed to read webhook body: #{inspect(reason)}")
        send_resp(conn, 400, "")
    end
  end

  # The fields Rust's ForgejoJobEvent requires, with its defaults for the optional ones.
  defp parse(%{"action" => action, "job" => %{} = job, "run" => %{} = run})
       when is_binary(action) do
    repo = run["repository"]
    runs_on = Map.get(job, "runs_on") || []
    handle = Map.get(job, "handle") || ""
    workflow = run["workflow_id"]

    cond do
      not is_integer(job["id"]) -> {:error, "job id missing"}
      not is_binary(job["name"]) -> {:error, "job name missing"}
      not is_binary(job["status"]) -> {:error, "job status missing"}
      not is_binary(handle) -> {:error, "job handle is not a string"}
      not (is_list(runs_on) and Enum.all?(runs_on, &is_binary/1)) -> {:error, "bad runs_on"}
      not (is_nil(workflow) or is_binary(workflow)) -> {:error, "bad workflow_id"}
      not repo?(repo) -> {:error, "run.repository missing"}
      true -> {:ok, %{job: job, handle: handle, runs_on: runs_on, repo: repo, workflow: workflow}}
    end
  end

  defp parse(_), do: {:error, "action, job or run missing"}

  defp repo?(%{"full_name" => f, "name" => n, "owner" => %{"login" => l}})
       when is_binary(f) and is_binary(n) and is_binary(l),
       do: true

  defp repo?(_), do: false

  defp handle_event(_status, event_type, %{handle: ""} = p, _config) do
    Logger.warning(
      "Forgejo job #{p.job["id"]} (#{inspect(p.job["name"])}) has no handle - ignoring #{event_type}"
    )

    200
  end

  defp handle_event("waiting", _event_type, p, config) do
    forge = Keyword.get(config, :forge, "forgejo")
    key = "#{forge}:#{p.handle}"
    required = Keyword.get(config, :required_labels, Labels.default_required())
    scopes = Keyword.get(config, :scopes, [])
    repo = p.repo

    Logger.info(
      "Received Forgejo waiting for job #{key} (#{inspect(p.job["name"])} in " <>
        "#{repo["full_name"]}) with labels #{inspect(p.runs_on)}"
    )

    with {:required, true} <- {:required, Labels.has_required?(p.runs_on, required)},
         {:scope, scope} when scope != nil <-
           {:scope, job_scope(scopes, repo["owner"]["login"], repo["name"])},
         {:ok, scope, group} <-
           Labels.resolve_scope(
             forge,
             p.runs_on,
             Keyword.get(config, :label_mappings, []),
             scope
           ) do
      attrs = %{
        job_key: key,
        forge: forge,
        job_labels: p.runs_on,
        agent_labels: p.runs_on,
        runner_scope: scope,
        runner_group: group,
        repository: repo["full_name"],
        job_name: p.job["name"],
        workflow_name: p.workflow,
        # the payload has no job created time
        queued_at: nil
      }

      # a job already queued (redelivery, or found by the scan) is kept as is
      case Jobs.add_pending(attrs) do
        {:ok, _} ->
          Logger.info("Persisted job #{key} to pending queue")
          broadcast({:new_job, key})
          200

        {:error, reason} ->
          Logger.warning(
            "Failed to persist job #{key} (#{inspect(reason)}) - returning 503 for retry"
          )

          503
      end
    else
      {:required, false} ->
        Logger.info(
          "Job #{key} missing required labels #{inspect(required)} (has #{inspect(p.runs_on)}) - ignoring"
        )

        200

      {:scope, nil} ->
        Logger.info(
          "Job #{key} is in #{repo["full_name"]}, which none of forge '#{forge}' scopes " <>
            "#{inspect(scopes)} cover - ignoring"
        )

        200

      :ignore ->
        Logger.info("No label mapping matches job #{key} labels #{inspect(p.runs_on)} - ignoring")
        200
    end
  end

  defp handle_event("running", _event_type, p, config) do
    # Forgejo's payload names no runner and no start time
    key = "#{Keyword.get(config, :forge, "forgejo")}:#{p.handle}"
    RunHistory.job_started(key, nil, DateTime.utc_now())
    broadcast({:job_started, key, nil})
    200
  end

  defp handle_event(status, _event_type, p, config) when status in @finished do
    key = "#{Keyword.get(config, :forge, "forgejo")}:#{p.handle}"
    Logger.debug("Forgejo job #{key} finished: #{status}")
    RunHistory.job_completed(key, status, DateTime.utc_now())
    broadcast({:job_completed, key})
    200
  end

  defp handle_event(_status, event_type, p, _config) do
    Logger.debug("Ignoring Forgejo #{event_type} for job #{p.handle}")
    200
  end

  @doc "Forgejo's signature: the plain hex HMAC-SHA256 of the raw body, compared in constant time."
  def valid_signature?(secret, body, hex) when is_binary(secret) and is_binary(hex) do
    case Base.decode16(hex, case: :mixed) do
      {:ok, sig} -> Plug.Crypto.secure_compare(:crypto.mac(:hmac, :sha256, secret, body), sig)
      :error -> false
    end
  end

  def valid_signature?(_secret, _body, _hex), do: false

  @doc """
  The configured scope a job in `owner/repo` registers its runner in: the repository if it's
  configured, else its owner (org or user), else the instance; nil when none covers it
  (case-insensitive, Rust forgejo_job_scope).
  """
  def job_scope(scopes, owner, repo) do
    o = String.downcase(owner)
    r = String.downcase(repo)

    Enum.find(scopes, &repo_match?(&1, o, r)) ||
      Enum.find(scopes, &owner_match?(&1, o)) ||
      Enum.find(scopes, &(&1 == :instance))
  end

  defp repo_match?({:repo, so, sr}, o, r),
    do: String.downcase(so) == o and String.downcase(sr) == r

  defp repo_match?(_, _, _), do: false

  defp owner_match?({kind, name}, o) when kind in [:org, :user], do: String.downcase(name) == o
  defp owner_match?(_, _), do: false

  defp header(conn, name), do: conn |> get_req_header(name) |> List.first("")

  defp broadcast(msg), do: Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "fleet", msg)
end
