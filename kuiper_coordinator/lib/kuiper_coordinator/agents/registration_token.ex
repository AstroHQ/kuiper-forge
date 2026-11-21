defmodule KuiperCoordinator.Agents.RegistrationToken do
  @moduledoc "Single-use registration token (`reg_` + 32 alphanumerics), exchanged for an agent cert."
  use Ecto.Schema

  @primary_key {:token, :string, autogenerate: false}
  @timestamps_opts [type: :utc_datetime_usec, updated_at: false]
  schema "registration_tokens" do
    field :expires_at, :utc_datetime_usec
    field :created_by, :string

    timestamps()
  end
end
