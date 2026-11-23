defmodule KuiperCoordinator.RustImport.Config do
  @moduledoc """
  Reads the Rust coordinator's config.toml (kuiper-forge/src/config.rs) into what Phoenix
  stores: forges (with their secrets resolved, the way the Rust coordinator resolves them),
  provisioning settings and label mappings.

  The Rust coordinator had one provisioning mode for every forge; Phoenix has one per forge.
  The mode is given out the way the per-forge-mode migration does: in webhook mode the webhook
  forge (`[provisioning.webhook] forge`, else the first GitHub forge, else the first) takes
  deliveries and the rest poll; in polling mode every forge polls; in fixed capacity the first
  forge fills the pools in `[runner_scope]` and the rest poll.

  Only the file is read: `KUIPER_*` environment overrides the Rust coordinator also takes are
  not, apart from a Forgejo forge's `token_env`.
  """
  alias KuiperCoordinator.Forge.RunnerScope

  @legacy_github "github"

  @type t :: %{forges: [map()], settings: map(), label_mappings: [map()]}

  @spec load(Path.t()) :: {:ok, t()} | {:error, String.t()}
  def load(path) do
    path = Path.expand(path)

    with {:ok, text} <- read(path),
         {:ok, toml} <- decode(text, path),
         {:ok, parsed} <- parse(toml, Path.dirname(path)) do
      {:ok, parsed}
    else
      {:error, msg} -> {:error, "#{path}: #{msg}"}
    end
  end

  defp read(path) do
    case File.read(path) do
      {:ok, text} -> {:ok, text}
      {:error, :enoent} -> {:error, "no such file"}
      {:error, reason} -> {:error, "can't read it (#{:file.format_error(reason)})"}
    end
  end

  defp decode(text, _path) do
    case Toml.decode(text) do
      {:ok, toml} -> {:ok, toml}
      # the reason can quote the line, which may hold a secret: give only its position
      {:error, {:invalid_toml, reason}} -> {:error, "isn't valid TOML (#{position(reason)})"}
      {:error, _} -> {:error, "isn't valid TOML"}
    end
  end

  defp position(reason) when is_binary(reason) do
    case Regex.run(~r/line \d+(, column \d+)?/, reason) do
      [pos | _] -> pos
      _ -> "parse error"
    end
  end

  defp position(_), do: "parse error"

  defp parse(toml, dir) do
    prov = Map.get(toml, "provisioning", %{})
    mode = Map.get(prov, "mode", "fixed_capacity")

    with {:ok, mode} <- mode(mode),
         {:ok, forges} <- forges(toml, dir),
         {:ok, forges} <- assign_modes(forges, mode, prov, toml),
         {:ok, section} <- {:ok, Map.get(prov, mode_key(mode)) || %{}},
         {:ok, mappings} <- label_mappings(Map.get(section, "label_mappings", [])) do
      {:ok,
       %{
         forges: forges,
         settings: %{
           idle_runner_timeout_s: Map.get(prov, "idle_runner_timeout_secs", 600),
           scan_lookback_hours: Map.get(section, "queued_scan_lookback_hours", 168),
           required_labels: Map.get(section, "required_labels", ["self-hosted"]),
           webhook_path: get_in(prov, ["webhook", "path"]) || "/webhook"
         },
         label_mappings: mappings
       }}
    end
  end

  defp mode(m) when m in ["fixed_capacity", "webhook", "polling"], do: {:ok, String.to_atom(m)}
  defp mode(m), do: {:error, "unknown provisioning mode #{inspect(m)}"}

  defp mode_key(:webhook), do: "webhook"
  defp mode_key(:polling), do: "polling"
  defp mode_key(:fixed_capacity), do: "none"

  defp forges(toml, dir) do
    list =
      case {Map.get(toml, "github"), Map.get(toml, "forges", [])} do
        {nil, []} -> {:error, "no forge configured: add a [github] or [[forges]] section"}
        {gh, []} -> {:ok, [Map.merge(gh, %{"name" => @legacy_github, "type" => "github"})]}
        {nil, forges} -> {:ok, forges}
        _ -> {:error, "configure forges with either [github] or [[forges]], not both"}
      end

    with {:ok, list} <- list do
      list
      |> Enum.reduce_while({:ok, []}, fn f, {:ok, acc} ->
        case forge(f, dir) do
          {:ok, attrs} -> {:cont, {:ok, [attrs | acc]}}
          {:error, msg} -> {:halt, {:error, msg}}
        end
      end)
      |> case do
        {:ok, acc} -> {:ok, Enum.reverse(acc)}
        err -> err
      end
    end
  end

  defp forge(%{"type" => "github", "name" => name} = f, dir) do
    with {:ok, key} <- private_key(f, name, dir) do
      {:ok, %{name: name, type: :github, app_id: f["app_id"], private_key: key}}
    end
  end

  defp forge(%{"type" => "forgejo", "name" => name} = f, dir) do
    with {:ok, token} <- forgejo_token(f, name, dir) do
      {:ok, %{name: name, type: :forgejo, url: f["url"], token: token, scopes: f["scopes"] || []}}
    end
  end

  defp forge(%{"name" => name}, _dir),
    do: {:error, "forge '#{name}' needs type github or forgejo"}

  defp forge(_, _dir), do: {:error, "every [[forges]] entry needs a name"}

  # config.rs private_key_content: inline (with \n escapes) first, then the file
  defp private_key(f, name, dir) do
    cond do
      is_binary(f["private_key"]) ->
        {:ok, String.replace(f["private_key"], "\\n", "\n")}

      is_binary(f["private_key_path"]) ->
        read_secret(f["private_key_path"], dir, name, "private_key_path")

      true ->
        {:error, "forge '#{name}' needs private_key or private_key_path"}
    end
  end

  # config.rs token_content: token_env, then token_path, then token
  defp forgejo_token(f, name, dir) do
    token =
      cond do
        is_binary(f["token_env"]) ->
          case System.get_env(f["token_env"]) do
            nil -> {:error, "forge '#{name}': token_env #{f["token_env"]} is not set"}
            v -> {:ok, v}
          end

        is_binary(f["token_path"]) ->
          read_secret(f["token_path"], dir, name, "token_path")

        is_binary(f["token"]) ->
          {:ok, f["token"]}

        true ->
          {:error, "forge '#{name}' needs token_env, token_path or token"}
      end

    case token do
      {:ok, t} ->
        if String.trim(t) == "",
          do: {:error, "forge '#{name}': the token is empty"},
          else: {:ok, String.trim(t)}

      err ->
        err
    end
  end

  # relative paths: as the Rust coordinator saw them (its working directory), else next to
  # the config file
  defp read_secret(path, dir, name, field) do
    candidates =
      if Path.type(path) == :absolute,
        do: [path],
        else: [Path.expand(path), Path.expand(path, dir)]

    case Enum.find(candidates, &File.regular?/1) do
      nil -> {:error, "forge '#{name}': #{field} #{path} isn't a readable file"}
      found -> {:ok, File.read!(found)}
    end
  end

  defp assign_modes(forges, :polling, prov, _toml) do
    interval = get_in(prov, ["polling", "interval_secs"]) || 120
    {:ok, Enum.map(forges, &Map.merge(&1, %{mode: :polling, poll_interval_s: max(interval, 10)}))}
  end

  defp assign_modes(forges, :webhook, prov, _toml) do
    webhook = Map.get(prov, "webhook") || %{}

    target =
      case webhook["forge"] do
        nil -> Enum.find(forges, &(&1.type == :github)) || hd(forges)
        name -> Enum.find(forges, &(&1.name == name))
      end

    cond do
      target == nil ->
        {:error,
         "provisioning.webhook.forge #{inspect(webhook["forge"])} isn't a configured forge"}

      not is_binary(webhook["secret"]) ->
        {:error, "webhook mode needs [provisioning.webhook] secret"}

      true ->
        {:ok,
         Enum.map(forges, fn f ->
           if f.name == target.name,
             do: Map.merge(f, %{mode: :webhook, webhook_secret: webhook["secret"]}),
             else: Map.put(f, :mode, :polling)
         end)}
    end
  end

  defp assign_modes([first | rest], :fixed_capacity, _prov, toml) do
    with {:ok, scope} <- scope(toml["runner_scope"]) do
      cond do
        first.type != :github ->
          {:error, "fixed-capacity pools register with the first forge, which must be GitHub"}

        scope == nil ->
          {:error, "fixed-capacity mode needs a [runner_scope]"}

        true ->
          {:ok,
           [
             Map.merge(first, %{mode: :fixed_capacity, runner_scope: scope})
             | Enum.map(rest, &Map.put(&1, :mode, :polling))
           ]}
      end
    end
  end

  defp label_mappings(list) when is_list(list) do
    Enum.reduce_while(Enum.with_index(list), {:ok, []}, fn {m, i}, {:ok, acc} ->
      case scope(m["runner_scope"]) do
        {:ok, scope} ->
          {:cont,
           {:ok,
            acc ++
              [
                %{
                  labels: m["labels"] || [],
                  runner_scope: scope,
                  runner_group: m["runner_group"],
                  forge: m["forge"],
                  position: i
                }
              ]}}

        {:error, msg} ->
          {:halt, {:error, "label mapping #{i + 1}: #{msg}"}}
      end
    end)
  end

  defp label_mappings(_), do: {:error, "label_mappings must be a list"}

  # serde form `{ type = "organization", name = "x" }`, and the older
  # `{ organization = { name = "x" } }` the docs still show
  @doc false
  def scope(nil), do: {:ok, nil}
  def scope(%{"type" => "organization", "name" => n}), do: {:ok, RunnerScope.to_string({:org, n})}

  def scope(%{"type" => "repository", "owner" => o, "repo" => r}),
    do: {:ok, RunnerScope.to_string({:repo, o, r})}

  def scope(%{"type" => "user", "name" => n}), do: {:ok, RunnerScope.to_string({:user, n})}
  def scope(%{"type" => "instance"}), do: {:ok, "*"}

  def scope(%{"organization" => %{"name" => n}}),
    do: scope(%{"type" => "organization", "name" => n})

  def scope(%{"repository" => %{"owner" => o, "repo" => r}}),
    do: scope(%{"type" => "repository", "owner" => o, "repo" => r})

  def scope(other), do: {:error, "unknown runner_scope #{inspect(Map.keys(other || %{}))}"}
end
