defmodule KuiperCoordinator.Admin.ApiToken do
  @moduledoc "An API token record (api_tokens.rs ApiToken): SHA-256 of the token, never the token."
  use Ecto.Schema

  @type t :: %__MODULE__{}

  @primary_key {:id, :binary_id, autogenerate: true}
  @timestamps_opts [type: :utc_datetime_usec, updated_at: false]
  schema "api_tokens" do
    field :name, :string
    field :token_hash, :binary, redact: true
    field :token_prefix, :string
    field :created_by, :string
    field :last_used_at, :utc_datetime_usec

    timestamps()
  end
end
