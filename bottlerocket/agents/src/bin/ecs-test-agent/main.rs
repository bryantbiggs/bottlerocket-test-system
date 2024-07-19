/*!

Tests whether an ECS task runs successfully.

!*/

use agent_utils::aws::aws_config;
use agent_utils::init_agent_logger;
use async_trait::async_trait;
use aws_sdk_ecs::error::SdkError as EcsSdkError;
use aws_sdk_ecs::operation::describe_task_definition::{
    DescribeTaskDefinitionError, DescribeTaskDefinitionOutput,
};
use aws_sdk_ecs::types::{Compatibility, ContainerDefinition, LaunchType, TaskStopCode};
use bottlerocket_agents::constants::DEFAULT_TASK_DEFINITION;
use bottlerocket_agents::error::{self, Error};
use bottlerocket_types::agent_config::{EcsTestConfig, AWS_CREDENTIALS_SECRET_NAME};
use log::info;
use snafu::{OptionExt, ResultExt};
use std::time::Duration;
use test_agent::{
    BootstrapData, ClientError, DefaultClient, DefaultInfoClient, InfoClient, Runner, Spec,
    TestAgent,
};
use testsys_model::{Outcome, SecretName, TestResults};

struct EcsTestRunner {
    config: EcsTestConfig,
    aws_secret_name: Option<SecretName>,
}

#[async_trait]
impl<I> Runner<I> for EcsTestRunner
where
    I: InfoClient,
{
    type C = EcsTestConfig;
    type E = Error;

    async fn new(spec: Spec<Self::C>, _info_client: &I) -> Result<Self, Self::E> {
        info!("Initializing Ecs test agent...");
        Ok(Self {
            config: spec.configuration,
            aws_secret_name: spec.secrets.get(AWS_CREDENTIALS_SECRET_NAME).cloned(),
        })
    }

    async fn run(&mut self, _info_client: &I) -> Result<TestResults, Self::E> {
        let config = aws_config(
            &self.aws_secret_name.as_ref(),
            &self.config.assume_role,
            &None,
            &self.config.region,
            &None,
            false,
        )
        .await?;
        let ecs_client = aws_sdk_ecs::Client::new(&config);

        info!("Waiting for registered container instances...");

        tokio::time::timeout(
            Duration::from_secs(30),
            wait_for_registered_containers(&ecs_client, &self.config.cluster_name),
        )
        .await
        .context(error::InstanceTimeoutSnafu)??;

        let task_name = match &self.config.task_definition_name_and_revision {
            Some(task_definition) => task_definition.clone(),
            None => create_or_find_task_definition(&ecs_client).await?,
        };

        info!("Running task '{}'", task_name);

        let run_task_output = ecs_client
            .run_task()
            .cluster(&self.config.cluster_name)
            .task_definition(task_name)
            .count(self.config.task_count)
            .launch_type(LaunchType::Ec2)
            .send()
            .await
            .context(error::TaskRunCreationSnafu)?;
        let task_arns: Vec<String> = run_task_output
            .tasks()
            .iter()
            .filter_map(|task| task.task_arn().map(|arn| arn.to_string()))
            .collect();

        info!("Waiting for tasks to complete...");

        match tokio::time::timeout(
            Duration::from_secs(30),
            wait_for_test_running(
                &ecs_client,
                &self.config.cluster_name,
                &task_arns,
                self.config.task_count,
            ),
        )
        .await
        {
            Ok(results) => results,
            Err(_) => {
                test_results(
                    &ecs_client,
                    &self.config.cluster_name,
                    &task_arns,
                    self.config.task_count,
                )
                .await
            }
        }
    }

    async fn terminate(&mut self) -> Result<(), Self::E> {
        Ok(())
    }
}

async fn wait_for_test_running(
    ecs_client: &aws_sdk_ecs::Client,
    cluster_name: &str,
    task_arns: &[String],
    task_count: i32,
) -> Result<TestResults, Error> {
    loop {
        let results = test_results(ecs_client, cluster_name, task_arns, task_count).await?;
        if results.outcome == Outcome::Pass {
            return Ok(results);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn test_results(
    ecs_client: &aws_sdk_ecs::Client,
    cluster_name: &str,
    task_arns: &[String],
    task_count: i32,
) -> Result<TestResults, Error> {
    let tasks = ecs_client
        .describe_tasks()
        .cluster(cluster_name)
        .set_tasks(Some(task_arns.to_vec()))
        .send()
        .await
        .context(error::TaskDescribeSnafu)?
        .tasks()
        .to_owned();
    let running_count = tasks
        .iter()
        .filter(|task| task.last_status() == Some("STOPPED"))
        .filter(|task| task.stop_code() == Some(&TaskStopCode::EssentialContainerExited))
        .filter(|task| {
            task.containers()
                .iter()
                .filter(|container| container.exit_code() != Some(0))
                .count()
                == 0
        })
        .count() as i32;
    Ok(TestResults {
        outcome: if task_count == running_count {
            Outcome::Pass
        } else {
            Outcome::Fail
        },
        num_passed: running_count as u64,
        num_failed: (task_count - running_count) as u64,
        num_skipped: 0,
        other_info: None,
    })
}

async fn wait_for_registered_containers(
    ecs_client: &aws_sdk_ecs::Client,
    cluster: &str,
) -> Result<(), Error> {
    loop {
        let cluster = ecs_client
            .describe_clusters()
            .clusters(cluster)
            .send()
            .await
            .context(error::ClusterDescribeSnafu)?
            .clusters()
            .first()
            .context(error::NoTaskSnafu)?
            .clone();

        if cluster.registered_container_instances_count() != 0 {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Retrieves the task_definition and revision of the testsys provided task definition. If the
/// task definition doesn't exist, it will be created.
async fn create_or_find_task_definition(ecs_client: &aws_sdk_ecs::Client) -> Result<String, Error> {
    let exists = exists(
        ecs_client
            .describe_task_definition()
            .task_definition(DEFAULT_TASK_DEFINITION)
            .send()
            .await,
    );
    if exists {
        latest_task_revision(ecs_client).await
    } else {
        create_task_definition(ecs_client).await
    }
}

/// Creates a task definition for testsys that runs a simple echo command to ensure the system
/// is running properly.
async fn create_task_definition(ecs_client: &aws_sdk_ecs::Client) -> Result<String, Error> {
    let task_info = ecs_client
        .register_task_definition()
        .family(DEFAULT_TASK_DEFINITION)
        .container_definitions(
            ContainerDefinition::builder()
                .name("ecs-smoke-test")
                .image("public.ecr.aws/amazonlinux/amazonlinux:2")
                .essential(true)
                .set_entry_point(Some(vec!["sh".to_string(), "-c".to_string()]))
                .command("/bin/sh -c \"echo hello-world\"")
                .build(),
        )
        .requires_compatibilities(Compatibility::Ec2)
        .cpu("256")
        .memory("512")
        .send()
        .await
        .context(error::TaskDefinitionCreationSnafu)?;
    let revision = task_info
        .task_definition()
        .context(error::TaskDefinitionMissingSnafu)?
        .revision();
    Ok(format!("{}:{}", DEFAULT_TASK_DEFINITION, revision))
}

/// Retrieve the task definition and the latest revision of the testsys provided ecs task definition.
async fn latest_task_revision(ecs_client: &aws_sdk_ecs::Client) -> Result<String, Error> {
    let task_info = ecs_client
        .describe_task_definition()
        .task_definition(DEFAULT_TASK_DEFINITION)
        .send()
        .await
        .context(error::TaskDefinitionDescribeSnafu)?;
    let revision = task_info
        .task_definition()
        .context(error::TaskDefinitionMissingSnafu)?
        .revision();
    Ok(format!("{}:{}", DEFAULT_TASK_DEFINITION, revision))
}

fn exists(
    result: Result<DescribeTaskDefinitionOutput, EcsSdkError<DescribeTaskDefinitionError>>,
) -> bool {
    if let Err(EcsSdkError::ServiceError(service_error)) = result {
        if matches!(
            &service_error.err(),
            DescribeTaskDefinitionError::ClientException(_)
        ) {
            return false;
        }
    }
    true
}

#[tokio::main]
async fn main() {
    init_agent_logger(env!("CARGO_CRATE_NAME"), None);
    if let Err(e) = run().await {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), test_agent::error::Error<ClientError, Error>> {
    let mut agent = TestAgent::<DefaultClient, EcsTestRunner, DefaultInfoClient>::new(
        BootstrapData::from_env().unwrap_or_else(|_| BootstrapData {
            test_name: "ecs_test".to_string(),
        }),
    )
    .await?;
    agent.run().await
}
