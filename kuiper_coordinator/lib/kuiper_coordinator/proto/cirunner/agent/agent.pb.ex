defmodule Cirunner.Agent.RunnerEvent.RunnerEventType do
  @moduledoc false

  use Protobuf,
    enum: true,
    full_name: "cirunner.agent.RunnerEvent.RunnerEventType",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :RUNNER_EVENT_TYPE_UNSPECIFIED, 0
  field :STARTED, 1
  field :COMPLETED, 2
  field :FAILED, 3
  field :DESTROYED, 4
end

defmodule Cirunner.Agent.RegisterRequest do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.RegisterRequest",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :registration_token, 1, type: :string, json_name: "registrationToken"
  field :hostname, 2, type: :string
  field :agent_type, 3, type: :string, json_name: "agentType"
  field :labels, 4, repeated: true, type: :string
  field :max_vms, 5, type: :uint32, json_name: "maxVms"
end

defmodule Cirunner.Agent.RegisterResponse do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.RegisterResponse",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :agent_id, 1, type: :string, json_name: "agentId"
  field :client_cert_pem, 2, type: :string, json_name: "clientCertPem"
  field :client_key_pem, 3, type: :string, json_name: "clientKeyPem"
  field :expires_at, 4, type: :string, json_name: "expiresAt"
  field :server_ca_pem, 5, type: :string, json_name: "serverCaPem"
  field :server_trust_mode, 6, type: :string, json_name: "serverTrustMode"
end

defmodule Cirunner.Agent.LogBatch do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.LogBatch",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :records, 1, repeated: true, type: Cirunner.Agent.LogRecord
  field :dropped, 2, type: :uint64
end

defmodule Cirunner.Agent.LogRecord do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.LogRecord",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :timestamp_micros, 1, type: :int64, json_name: "timestampMicros"
  field :level, 2, type: :string
  field :target, 3, type: :string
  field :message, 4, type: :string
  field :seq, 5, type: :uint64
end

defmodule Cirunner.Agent.UploadLogsResponse do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.UploadLogsResponse",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3
end

defmodule Cirunner.Agent.AgentMessage do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.AgentMessage",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  oneof(:payload, 0)

  field :status, 1, type: Cirunner.Agent.AgentStatus, oneof: 0
  field :result, 2, type: Cirunner.Agent.CommandResult, oneof: 0
  field :pong, 3, type: Cirunner.Agent.Pong, oneof: 0
  field :ack, 4, type: Cirunner.Agent.CommandAck, oneof: 0
  field :runner_event, 5, type: Cirunner.Agent.RunnerEvent, json_name: "runnerEvent", oneof: 0
end

defmodule Cirunner.Agent.AgentStatus do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.AgentStatus",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :active_vms, 1, type: :uint32, json_name: "activeVms"
  field :available_slots, 2, type: :uint32, json_name: "availableSlots"
  field :vms, 3, repeated: true, type: Cirunner.Agent.VmInfo
  field :agent_id, 4, type: :string, json_name: "agentId"
  field :hostname, 5, type: :string
  field :agent_type, 6, type: :string, json_name: "agentType"
  field :labels, 7, repeated: true, type: :string
  field :max_vms, 8, type: :uint32, json_name: "maxVms"
  field :label_sets, 9, repeated: true, type: Cirunner.Agent.LabelSet, json_name: "labelSets"
  field :agent_version, 10, type: :string, json_name: "agentVersion"
  field :limits, 11, repeated: true, type: Cirunner.Agent.CapacityLimit
  field :runner_kinds, 12, repeated: true, type: :string, json_name: "runnerKinds"
end

defmodule Cirunner.Agent.CapacityLimit do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.CapacityLimit",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :name, 1, type: :string
  field :max, 2, type: :uint32
  field :external, 3, type: :uint32
  field :active, 4, type: :uint32
end

defmodule Cirunner.Agent.LabelSet do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.LabelSet",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :labels, 1, repeated: true, type: :string
  field :limits, 2, repeated: true, type: :string
  field :pool_size, 3, proto3_optional: true, type: :uint32, json_name: "poolSize"
  field :id, 4, type: :string
  field :is_default, 5, type: :bool, json_name: "isDefault"
end

defmodule Cirunner.Agent.VmInfo do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.VmInfo",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :vm_id, 1, type: :string, json_name: "vmId"
  field :name, 2, type: :string
  field :state, 3, type: :string
  field :ip_address, 4, type: :string, json_name: "ipAddress"
end

defmodule Cirunner.Agent.CommandAck do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.CommandAck",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :command_id, 1, type: :string, json_name: "commandId"
  field :accepted, 2, type: :bool
  field :error, 3, type: :string
end

defmodule Cirunner.Agent.RunnerEvent do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.RunnerEvent",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :runner_name, 1, type: :string, json_name: "runnerName"
  field :vm_id, 2, type: :string, json_name: "vmId"

  field :event_type, 3,
    type: Cirunner.Agent.RunnerEvent.RunnerEventType,
    json_name: "eventType",
    enum: true

  field :error, 4, type: :string
  field :timings, 5, type: Cirunner.Agent.RunnerTimings
end

defmodule Cirunner.Agent.RunnerTimings.StepsMsEntry do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.RunnerTimings.StepsMsEntry",
    map: true,
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :key, 1, type: :string
  field :value, 2, type: :uint64
end

defmodule Cirunner.Agent.RunnerTimings do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.RunnerTimings",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :steps_ms, 1,
    repeated: true,
    type: Cirunner.Agent.RunnerTimings.StepsMsEntry,
    json_name: "stepsMs",
    map: true

  field :failed_phase, 2, type: :string, json_name: "failedPhase"
  field :image, 3, type: :string
  field :install_source, 4, type: :string, json_name: "installSource"
end

defmodule Cirunner.Agent.CommandResult do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.CommandResult",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  oneof(:result, 0)

  field :command_id, 1, type: :string, json_name: "commandId"
  field :success, 2, type: :bool
  field :error, 3, type: :string

  field :create_runner, 4,
    type: Cirunner.Agent.CreateRunnerResult,
    json_name: "createRunner",
    oneof: 0

  field :destroy_runner, 5,
    type: Cirunner.Agent.DestroyRunnerResult,
    json_name: "destroyRunner",
    oneof: 0
end

defmodule Cirunner.Agent.CreateRunnerResult do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.CreateRunnerResult",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :vm_id, 1, type: :string, json_name: "vmId"
  field :ip_address, 2, type: :string, json_name: "ipAddress"
end

defmodule Cirunner.Agent.DestroyRunnerResult do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.DestroyRunnerResult",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3
end

defmodule Cirunner.Agent.Pong do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.Pong",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3
end

defmodule Cirunner.Agent.CoordinatorMessage do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.CoordinatorMessage",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  oneof(:payload, 0)

  field :create_runner, 1,
    type: Cirunner.Agent.CreateRunnerCommand,
    json_name: "createRunner",
    oneof: 0

  field :destroy_runner, 2,
    type: Cirunner.Agent.DestroyRunnerCommand,
    json_name: "destroyRunner",
    oneof: 0

  field :ping, 3, type: Cirunner.Agent.Ping, oneof: 0
end

defmodule Cirunner.Agent.CreateRunnerCommand do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.CreateRunnerCommand",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :command_id, 1, type: :string, json_name: "commandId"
  field :vm_name, 2, type: :string, json_name: "vmName"
  field :registration_token, 3, type: :string, json_name: "registrationToken"
  field :labels, 4, repeated: true, type: :string
  field :runner_scope_url, 5, type: :string, json_name: "runnerScopeUrl"
  field :jit_config, 6, type: :string, json_name: "jitConfig"
  field :label_set_id, 7, type: :string, json_name: "labelSetId"
  field :forgejo, 8, type: Cirunner.Agent.ForgejoRunner
end

defmodule Cirunner.Agent.ForgejoRunner do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.ForgejoRunner",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :instance_url, 1, type: :string, json_name: "instanceUrl"
  field :uuid, 2, type: :string
  field :token, 3, type: :string
  field :handle, 4, type: :string
end

defmodule Cirunner.Agent.DestroyRunnerCommand do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.DestroyRunnerCommand",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3

  field :command_id, 1, type: :string, json_name: "commandId"
  field :vm_id, 2, type: :string, json_name: "vmId"
end

defmodule Cirunner.Agent.Ping do
  @moduledoc false

  use Protobuf,
    full_name: "cirunner.agent.Ping",
    protoc_gen_elixir_version: "0.17.1",
    syntax: :proto3
end

defmodule Cirunner.Agent.RegistrationService.Service do
  @moduledoc false

  use GRPC.Service,
    name: "cirunner.agent.RegistrationService",
    protoc_gen_elixir_version: "0.17.1"

  rpc(:Register, Cirunner.Agent.RegisterRequest, Cirunner.Agent.RegisterResponse)
end

defmodule Cirunner.Agent.RegistrationService.Stub do
  @moduledoc false

  use GRPC.Stub, service: Cirunner.Agent.RegistrationService.Service
end

defmodule Cirunner.Agent.AgentService.Service do
  @moduledoc false

  use GRPC.Service, name: "cirunner.agent.AgentService", protoc_gen_elixir_version: "0.17.1"

  rpc(
    :AgentStream,
    stream(Cirunner.Agent.AgentMessage),
    stream(Cirunner.Agent.CoordinatorMessage)
  )

  rpc(:UploadLogs, Cirunner.Agent.LogBatch, Cirunner.Agent.UploadLogsResponse)
end

defmodule Cirunner.Agent.AgentService.Stub do
  @moduledoc false

  use GRPC.Stub, service: Cirunner.Agent.AgentService.Service
end
