defmodule KuiperCoordinator.Forges.Forge do
  @moduledoc """
  A stored forge (D39): a GitHub App (`app_id`, `private_key`) or a Forgejo instance (`url`,
  `token`, `scopes`). `private_key`, `token` and `webhook_secret` are encrypted at rest
  (`KuiperCoordinator.Vault`) and never shown back: on update, leave a secret out (or blank) to
  keep the stored one.

  Each forge has its own provisioning `mode`:

    * `:webhook` - jobs come from the forge's deliveries (needs `webhook_secret`), with a scan
      now and then for ones that were missed.
    * `:polling` - jobs come from scanning the forge every `poll_interval_s` (default 120, at
      least 10).
    * `:fixed_capacity` - the agents' pools are kept full of runners registered in
      `runner_scope` (an org or owner/repo). GitHub only.
  """
  use Ecto.Schema
  import Ecto.Changeset

  alias KuiperCoordinator.Encrypted
  alias KuiperCoordinator.Forge.RunnerScope

  @secrets [:private_key, :token, :webhook_secret]
  @modes [:webhook, :polling, :fixed_capacity]
  @min_poll_s 10

  schema "forges" do
    field :name, :string
    field :type, Ecto.Enum, values: [:github, :forgejo]
    field :app_id, :integer
    field :private_key, Encrypted.Binary, redact: true
    field :url, :string
    field :token, Encrypted.Binary, redact: true
    field :scopes, {:array, :string}, default: []
    field :webhook_secret, Encrypted.Binary, redact: true
    field :mode, Ecto.Enum, values: @modes, default: :polling
    field :runner_scope, :string
    field :poll_interval_s, :integer, default: 120
    timestamps(type: :utc_datetime_usec)
  end

  @doc "The secret fields, which the UI never fills back in."
  def secret_fields, do: @secrets

  def modes, do: @modes

  @doc """
  Checks the fields for the forge's type (C2, C10, FJ7, F13) and its mode. `type` is fixed once
  saved. Blank secrets on an existing forge keep the stored ones.
  """
  def changeset(%__MODULE__{} = forge, attrs) do
    forge
    |> cast(
      attrs,
      [
        :name,
        :type,
        :app_id,
        :private_key,
        :url,
        :token,
        :scopes,
        :webhook_secret,
        :mode,
        :runner_scope,
        :poll_interval_s
      ],
      empty_values: [nil, ""]
    )
    |> keep_secrets(forge)
    |> validate_required([:name, :type, :mode, :poll_interval_s])
    |> validate_format(:name, ~r/^[^:\s]+$/, message: "can't contain ':' or spaces")
    |> validate_length(:name, max: 64)
    |> unique_constraint(:name)
    |> fixed_type(forge)
    |> by_type()
    |> validate_number(:poll_interval_s, greater_than_or_equal_to: @min_poll_s)
    |> validate_change(:runner_scope, fn :runner_scope, scope ->
      case RunnerScope.parse(scope) do
        {:ok, _} -> []
        {:error, msg} -> [runner_scope: msg]
      end
    end)
    |> by_mode()
  end

  defp by_mode(cs) do
    case get_field(cs, :mode) do
      :webhook ->
        if get_field(cs, :webhook_secret) in [nil, ""],
          do: add_error(cs, :webhook_secret, "is needed in webhook mode"),
          else: cs

      # C3: pools are filled with registration-token runners in one GitHub org or repo
      :fixed_capacity ->
        scope = get_field(cs, :runner_scope)

        cond do
          get_field(cs, :type) != :github ->
            add_error(cs, :mode, "fixed capacity is for GitHub forges; use webhook or polling")

          scope == nil ->
            add_error(cs, :runner_scope, "is needed for fixed capacity (org or owner/repo)")

          not github_scope?(scope) ->
            add_error(cs, :runner_scope, "must be an org or owner/repo")

          true ->
            cs
        end

      _ ->
        cs
    end
  end

  defp github_scope?(scope) do
    case RunnerScope.parse(scope) do
      {:ok, parsed} -> RunnerScope.github?(parsed)
      # a scope that doesn't parse already has its error
      _ -> true
    end
  end

  # a blank secret on update means "keep the stored one"
  defp keep_secrets(cs, %__MODULE__{id: nil}), do: cs

  defp keep_secrets(cs, forge) do
    Enum.reduce(@secrets, cs, fn f, cs ->
      if get_change(cs, f) == nil and Map.get(forge, f) != nil, do: delete_change(cs, f), else: cs
    end)
  end

  defp fixed_type(cs, %__MODULE__{id: nil}), do: cs

  defp fixed_type(cs, _forge) do
    if get_change(cs, :type), do: add_error(cs, :type, "can't be changed"), else: cs
  end

  defp by_type(cs) do
    case get_field(cs, :type) do
      :github ->
        cs
        |> update_change(:private_key, &String.replace(&1, "\\n", "\n"))
        |> validate_required([:app_id, :private_key])
        |> validate_number(:app_id, greater_than: 0)
        |> validate_change(:private_key, fn :private_key, pem ->
          if String.contains?(pem, "PRIVATE KEY"),
            do: [],
            else: [private_key: "must be a PEM private key"]
        end)

      :forgejo ->
        cs
        |> update_change(:token, &String.trim/1)
        |> update_change(:url, &String.trim_trailing(String.trim(&1), "/"))
        |> update_change(:scopes, fn s ->
          s |> Enum.map(&String.trim/1) |> Enum.reject(&(&1 == ""))
        end)
        |> validate_required([:url, :token])
        |> validate_change(:url, &http_url/2)
        |> validate_scopes()

      _ ->
        cs
    end
  end

  defp http_url(:url, url) do
    case URI.parse(url) do
      %URI{scheme: s, host: h} when s in ["http", "https"] and is_binary(h) and h != "" -> []
      _ -> [url: "must be an http(s) URL"]
    end
  end

  # F13: the same rules as Rust's runner_scopes
  defp validate_scopes(cs) do
    case get_field(cs, :scopes) do
      [] ->
        add_error(cs, :scopes, "need at least one (org, owner/repo, @user or *)")

      scopes ->
        Enum.reduce(scopes, cs, fn s, cs ->
          case RunnerScope.parse(s) do
            {:ok, _} -> cs
            {:error, msg} -> add_error(cs, :scopes, msg)
          end
        end)
    end
  end
end
