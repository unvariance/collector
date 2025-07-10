use std::future::Future;
use tokio_util::sync::CancellationToken;

/// Task completion handler that manages task lifecycle and cancellation
///
/// This handler wraps any future that returns a Result and ensures:
/// 1. Proper logging of success, errors, and panics
/// 2. Cancellation token is triggered when task completes for any reason
/// 3. Graceful handling of all task completion scenarios
pub async fn task_completion_handler<F, T, E>(future: F, token: CancellationToken, task_name: &str)
where
    F: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static + std::fmt::Debug,
{
    let handle = tokio::spawn(future);

    match handle.await {
        Ok(Ok(_)) => {
            // Task completed successfully
            log::debug!("{} completed successfully", task_name);
        }
        Ok(Err(error)) => {
            // Task completed but returned an error
            log::error!("{} failed with error: {:?}", task_name, error);
        }
        Err(join_error) => {
            // Task panicked or was cancelled
            log::error!("{} panicked or was cancelled: {:?}", task_name, join_error);
        }
    }

    // Always cancel the token when task completes for any reason
    token.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    #[allow(dead_code)]
    struct TestError(String);

    #[tokio::test]
    async fn test_successful_completion() {
        testing_logger::setup();

        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Create a future that succeeds
        let future = async { Ok::<(), TestError>(()) };

        // Run the completion handler
        task_completion_handler(future, token, "test_task").await;

        // Verify token was cancelled
        assert!(token_clone.is_cancelled());

        // Verify log output
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].level, log::Level::Debug);
            assert_eq!(captured_logs[0].body, "test_task completed successfully");
        });
    }

    #[tokio::test]
    async fn test_error_completion() {
        testing_logger::setup();

        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Create a future that returns an error
        let future = async { Err::<(), TestError>(TestError("test error".to_string())) };

        // Run the completion handler
        task_completion_handler(future, token, "error_task").await;

        // Verify token was cancelled
        assert!(token_clone.is_cancelled());

        // Verify log output
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].level, log::Level::Error);
            assert_eq!(
                captured_logs[0].body,
                "error_task failed with error: TestError(\"test error\")"
            );
        });
    }

    #[tokio::test]
    async fn test_panic_completion() {
        testing_logger::setup();

        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Create a future that panics
        let future = async {
            panic!("test panic");
            #[allow(unreachable_code)]
            Ok::<(), TestError>(())
        };

        // Run the completion handler
        task_completion_handler(future, token, "panic_task").await;

        // Verify token was cancelled
        assert!(token_clone.is_cancelled());

        // Verify log output
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].level, log::Level::Error);
            assert!(captured_logs[0]
                .body
                .starts_with("panic_task panicked or was cancelled:"));
            assert!(captured_logs[0].body.contains("test panic"));
        });
    }

    #[tokio::test]
    async fn test_multiple_handlers_independent_cancellation() {
        testing_logger::setup();

        let token1 = CancellationToken::new();
        let token2 = CancellationToken::new();
        let token1_clone = token1.clone();
        let token2_clone = token2.clone();

        // Create two futures, one succeeds, one fails
        let future1 = async { Ok::<(), TestError>(()) };
        let future2 = async { Err::<(), TestError>(TestError("multi_error".to_string())) };

        // Run both handlers
        let (_, _) = tokio::join!(
            task_completion_handler(future1, token1, "multi_task1"),
            task_completion_handler(future2, token2, "multi_task2")
        );

        // Verify both tokens were cancelled independently
        assert!(token1_clone.is_cancelled());
        assert!(token2_clone.is_cancelled());

        // Verify log output for both tasks
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 2);

            // Find the success log
            let success_log = captured_logs
                .iter()
                .find(|log| log.body.contains("multi_task1 completed successfully"))
                .expect("Should have success log");
            assert_eq!(success_log.level, log::Level::Debug);

            // Find the error log
            let error_log = captured_logs
                .iter()
                .find(|log| log.body.contains("multi_task2 failed with error"))
                .expect("Should have error log");
            assert_eq!(error_log.level, log::Level::Error);
            assert!(error_log.body.contains("TestError(\"multi_error\")"));
        });
    }

    #[tokio::test]
    async fn test_task_name_variations() {
        testing_logger::setup();

        let test_cases = vec![
            ("simple_task", "simple_task completed successfully"),
            (
                "complex-task-name",
                "complex-task-name completed successfully",
            ),
            (
                "TaskWithCamelCase",
                "TaskWithCamelCase completed successfully",
            ),
            ("task.with.dots", "task.with.dots completed successfully"),
        ];

        for (task_name, _expected_message) in test_cases.iter() {
            let token = CancellationToken::new();
            let future = async { Ok::<(), TestError>(()) };

            task_completion_handler(future, token.clone(), task_name).await;

            // Each task should have cancelled its token
            assert!(token.is_cancelled());
        }

        // Verify all log messages
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 4);

            for (i, (_, expected_message)) in test_cases.iter().enumerate() {
                assert_eq!(captured_logs[i].level, log::Level::Debug);
                assert_eq!(captured_logs[i].body, *expected_message);
            }
        });
    }

    #[tokio::test]
    async fn test_complex_error_types() {
        testing_logger::setup();

        #[derive(Debug)]
        #[allow(dead_code)]
        struct ComplexError {
            code: u32,
            message: String,
        }

        let token = CancellationToken::new();
        let token_clone = token.clone();

        let future = async {
            Err::<(), ComplexError>(ComplexError {
                code: 404,
                message: "Resource not found".to_string(),
            })
        };

        task_completion_handler(future, token, "complex_task").await;

        // Verify token was cancelled
        assert!(token_clone.is_cancelled());

        // Verify log output includes complex error details
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].level, log::Level::Error);
            assert!(captured_logs[0]
                .body
                .starts_with("complex_task failed with error:"));
            assert!(captured_logs[0].body.contains("code: 404"));
            assert!(captured_logs[0].body.contains("Resource not found"));
        });
    }
}
