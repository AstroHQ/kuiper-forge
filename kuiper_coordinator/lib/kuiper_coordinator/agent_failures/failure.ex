defmodule KuiperCoordinator.AgentFailures.Failure do
  @moduledoc "One recorded agent failure (Rust agent_failures). `kind` is a string so unknown kinds survive."
  use Ecto.Schema

  @primary_key {:id, :binary_id, autogenerate: true}
  schema "agent_failures" do
    field :agent_id, :string
    field :occurred_at, :utc_datetime_usec
    field :kind, :string
    field :runner_name, :string
    field :job_key, :string
    field :message, :string
  end
end
