defmodule KuiperCoordinatorWeb.Plugs.GitHubWebhook do
  @moduledoc """
  GitHub `workflow_job` webhooks (parity W1-W3, W5-W7), ported from kuiper-forge/src/webhook.rs.

  Sits in the endpoint before `Plug.Parsers` so it reads the raw body: the HMAC is over the
  exact bytes, and the status codes come in Rust's order: 200 for ignored events (before the
  signature), 401 for a bad signature, 400 for bad JSON, 503 when the job can't be persisted
  (GitHub redelivers). A persisted job is announced on PubSub topic `"fleet"`.

  Config (`config :kuiper_coordinator, :webhook`): `:sources`, one per forge in webhook mode
  (`%{forge: name, source: :github | :forgejo, secret: secret}`, plus `:scopes` for Forgejo;
  none turns the plug off), `:path` (default `/webhook`), `:required_labels` (default
  `["self-hosted"]`), `:label_mappings`, `:max_body` (bytes, default 25 MB; larger is 413).

  Every forge delivers to the same path. A delivery with `X-Forgejo-Event` goes to
  `KuiperCoordinatorWeb.Plugs.ForgejoWebhook`; the forge it's from is the source of that kind
  whose secret signed it (`source_for/3`), so a signature no source made is a 401.
  """
  @behaviour Plug
  import Plug.Conn
  require Logger

  alias KuiperCoordinator.{Jobs, RunHistory, Webhooks.Labels}

  @max_body 25 * 1024 * 1024

  @impl true
  def init(opts), do: opts

  @impl true
  def call(%Plug.Conn{method: "POST"} = conn, _opts) do
    config = Application.get_env(:kuiper_coordinator, :webhook, [])

    if Keyword.get(config, :sources, []) != [] and
         conn.request_path == Keyword.get(config, :path, "/webhook") do
      case get_req_header(conn, "x-forgejo-event") do
        [] -> conn |> handle(config) |> halt()
        _ -> conn |> KuiperCoordinatorWeb.Plugs.ForgejoWebhook.handle(config) |> halt()
      end
    else
      conn
    end
  end

  def call(conn, _opts), do: conn

  defp handle(conn, config) do
    with {:event, ["workflow_job"]} <- {:event, get_req_header(conn, "x-github-event")},
         {:ok, body, conn} <- read_full_body(conn, Keyword.get(config, :max_body, @max_body)),
         {:sig, %{forge: forge}} <-
           {:sig,
            source_for(config, :github, fn secret ->
              valid_signature?(secret, body, header(conn, "x-hub-signature-256"))
            end)},
         {:json, {:ok, %{} = event}} <- {:json, Jason.decode(body)} do
      send_resp(conn, handle_event(event, Keyword.put(config, :forge, forge)), "")
    else
      {:event, other} ->
        Logger.debug("Ignoring webhook event type: #{inspect(other)}")
        send_resp(conn, 200, "")

      {:sig, nil} ->
        Logger.warning("Webhook signature validation failed")
        send_resp(conn, 401, "")

      {:json, _} ->
        send_resp(conn, 400, "")

      {:error, :too_large} ->
        send_resp(conn, 413, "")

      {:error, reason} ->
        Logger.warning("Failed to read webhook body: #{inspect(reason)}")
        send_resp(conn, 400, "")
    end
  end

  # A validly signed but malformed workflow_job is a 400, never a crash or 5xx (GitHub would
  # redeliver a 5xx forever).
  defp handle_event(%{"workflow_job" => job} = event, config) do
    case malformed(event, job) do
      nil ->
        dispatch_event(event, config)

      why ->
        Logger.warning("Malformed workflow_job webhook (#{why}) - rejecting with 400")
        400
    end
  end

  defp handle_event(_event, _config), do: 200

  defp malformed(event, job) do
    cond do
      not is_map(job) ->
        "workflow_job is not an object"

      not (is_nil(event["repository"]) or is_map(event["repository"])) ->
        "repository is not an object"

      not (is_integer(job["id"]) or is_binary(job["id"])) ->
        "job id missing"

      not (is_nil(job["labels"]) or
               (is_list(job["labels"]) and Enum.all?(job["labels"], &is_binary/1))) ->
        "labels are not strings"

      not (is_nil(job["created_at"]) or is_binary(job["created_at"])) ->
        "created_at is not a string"

      event["action"] == "queued" and not is_binary(owner(event)) ->
        "no organization or repository owner"

      true ->
        nil
    end
  end

  defp owner(event) do
    case event do
      %{"organization" => %{"login" => login}} when is_binary(login) -> login
      %{"repository" => %{"owner" => %{"login" => login}}} -> login
      _ -> nil
    end
  end

  defp dispatch_event(%{"action" => "queued", "workflow_job" => job} = event, config) do
    forge = Keyword.get(config, :forge, "github")
    key = "#{forge}:#{job["id"]}"
    labels = job["labels"] || []
    required = Keyword.get(config, :required_labels, Labels.default_required())

    default_scope = {:org, owner(event)}

    with {:required, true} <- {:required, Labels.has_required?(labels, required)},
         {:ok, scope, group} <-
           Labels.resolve_scope(
             forge,
             labels,
             Keyword.get(config, :label_mappings, []),
             default_scope
           ) do
      attrs = %{
        job_key: key,
        forge: forge,
        job_labels: labels,
        agent_labels: labels,
        runner_scope: scope,
        runner_group: group,
        repository: get_in(event, ["repository", "full_name"]),
        job_name: job["name"],
        workflow_name: event["workflow_name"],
        queued_at: parse_time(job["created_at"])
      }

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
          "Job #{key} missing required labels #{inspect(required)} (has #{inspect(labels)}) - ignoring"
        )

        200

      :ignore ->
        Logger.info("No label mapping matches job #{key} labels #{inspect(labels)} - ignoring")
        200
    end
  end

  defp dispatch_event(%{"action" => "in_progress", "workflow_job" => job}, config) do
    key = "#{Keyword.get(config, :forge, "github")}:#{job["id"]}"
    # run history: the queue wait ends here, and it says which runner took the job
    at = parse_time(job["started_at"]) || DateTime.utc_now()
    RunHistory.job_started(key, job["runner_name"], at)
    if job["runner_name"], do: RunHistory.runner_job_started(job["runner_name"], at)
    broadcast({:job_started, key, job["runner_name"]})
    200
  end

  defp dispatch_event(%{"action" => action, "workflow_job" => job}, config)
       when action in ["completed", "cancelled"] do
    key = "#{Keyword.get(config, :forge, "github")}:#{job["id"]}"
    at = parse_time(job["completed_at"]) || DateTime.utc_now()
    RunHistory.job_completed(key, job["conclusion"], at)
    broadcast({:job_completed, key})
    200
  end

  defp dispatch_event(_event, _config), do: 200

  @doc "The source of `kind` whose secret passes `valid?`, the forge a delivery is from. Otherwise nil."
  def source_for(config, kind, valid?) do
    config
    |> Keyword.get(:sources, [])
    |> Enum.find(&(Map.get(&1, :source, :github) == kind and valid?.(&1[:secret])))
  end

  @doc "HMAC-SHA256 of the raw body against a `sha256=<hex>` header, in constant time."
  def valid_signature?(secret, body, "sha256=" <> hex) when is_binary(secret) do
    case Base.decode16(hex, case: :mixed) do
      {:ok, sig} -> Plug.Crypto.secure_compare(:crypto.mac(:hmac, :sha256, secret, body), sig)
      :error -> false
    end
  end

  def valid_signature?(_secret, _body, _header), do: false

  defp header(conn, name), do: conn |> get_req_header(name) |> List.first("")

  defp broadcast(msg), do: Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "fleet", msg)

  defp parse_time(nil), do: nil

  defp parse_time(s) do
    case DateTime.from_iso8601(s) do
      {:ok, dt, _} -> dt
      _ -> nil
    end
  end

  @doc false
  def read_full_body(conn, max, acc \\ []) do
    case read_body(conn) do
      {:ok, chunk, conn} ->
        if IO.iodata_length([acc, chunk]) > max,
          do: {:error, :too_large},
          else: {:ok, IO.iodata_to_binary([acc, chunk]), conn}

      {:more, chunk, conn} ->
        if IO.iodata_length([acc, chunk]) > max,
          do: {:error, :too_large},
          else: read_full_body(conn, max, [acc, chunk])

      {:error, _} = e ->
        e
    end
  end
end
