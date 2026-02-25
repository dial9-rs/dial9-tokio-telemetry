use aws_config::SdkConfig;
use aws_sdk_dynamodb::{
    Client,
    error::{BoxError, SdkError},
    types::{
        AttributeDefinition, AttributeValue, BillingMode, KeySchemaElement, KeyType,
        ScalarAttributeType, TableStatus,
    },
};

pub struct DdbClient {
    client: Client,
    table: String,
}

impl DdbClient {
    pub fn new(config: &SdkConfig, table: &str) -> Self {
        Self {
            client: Client::new(config),
            table: table.to_string(),
        }
    }

    pub async fn ensure_table(&self) -> Result<(), BoxError> {
        match self
            .client
            .describe_table()
            .table_name(&self.table)
            .send()
            .await
        {
            Ok(resp) => {
                if resp.table().and_then(|t| t.table_status()) != Some(&TableStatus::Active) {
                    self.wait_for_active().await?;
                }
                return Ok(());
            }
            Err(SdkError::ServiceError(e)) if e.err().is_resource_not_found_exception() => {}
            Err(e) => return Err(e.into()),
        }

        println!("Creating DynamoDB table '{}'...", self.table);
        self.client
            .create_table()
            .table_name(&self.table)
            .billing_mode(BillingMode::PayPerRequest)
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("metric_name")
                    .attribute_type(ScalarAttributeType::S)
                    .build()?,
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("timestamp")
                    .attribute_type(ScalarAttributeType::N)
                    .build()?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("metric_name")
                    .key_type(KeyType::Hash)
                    .build()?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("timestamp")
                    .key_type(KeyType::Range)
                    .build()?,
            )
            .send()
            .await?;

        self.wait_for_active().await
    }

    async fn wait_for_active(&self) -> Result<(), BoxError> {
        loop {
            let status = self
                .client
                .describe_table()
                .table_name(&self.table)
                .send()
                .await?
                .table()
                .and_then(|t| t.table_status().cloned());

            if status == Some(TableStatus::Active) {
                println!("Table '{}' is active.", self.table);
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    pub async fn put_aggregate(
        &self,
        name: &str,
        timestamp: u64,
        sum: f64,
        count: u64,
        min: f64,
        max: f64,
    ) -> Result<(), BoxError> {
        self.client
            .put_item()
            .table_name(&self.table)
            .item("metric_name", AttributeValue::S(name.to_string()))
            .item("timestamp", AttributeValue::N(timestamp.to_string()))
            .item("sum", AttributeValue::N(sum.to_string()))
            .item("count", AttributeValue::N(count.to_string()))
            .item("min", AttributeValue::N(min.to_string()))
            .item("max", AttributeValue::N(max.to_string()))
            .send()
            .await?;
        Ok(())
    }

    pub async fn query_metric(
        &self,
        name: &str,
    ) -> Result<Vec<(u64, f64, u64, f64, f64)>, BoxError> {
        let resp = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("metric_name = :name")
            .expression_attribute_values(":name", AttributeValue::S(name.to_string()))
            .scan_index_forward(false)
            .limit(20)
            .send()
            .await?;

        let items = resp.items.unwrap_or_default().into_iter().map(|item| {
            let ts = item["timestamp"].as_n().unwrap().parse().unwrap_or(0);
            let sum = item["sum"].as_n().unwrap().parse().unwrap_or(0.0);
            let count = item["count"].as_n().unwrap().parse().unwrap_or(0);
            let min = item["min"].as_n().unwrap().parse().unwrap_or(0.0);
            let max = item["max"].as_n().unwrap().parse().unwrap_or(0.0);
            (ts, sum, count, min, max)
        });

        Ok(items.collect())
    }
}
