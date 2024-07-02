#![allow(clippy::enum_variant_names)]

pub use sea_orm_migration::prelude::*;
pub use sea_orm_migration::MigrationStatus;
mod m20230908_072257_init;
mod m20231008_020431_hummock;
mod m20240304_074901_subscription;
mod m20240410_082733_with_version_column_migration;
mod m20240410_154406_session_params;
mod m20240417_062305_subscription_internal_table_name;
mod m20240418_142249_function_runtime;
mod m20240506_112555_subscription_partial_ckpt;
mod m20240525_090457_secret;
mod m20240617_070131_index_column_properties;
mod m20240618_072634_function_compressed_binary;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20230908_072257_init::Migration),
            Box::new(m20231008_020431_hummock::Migration),
            Box::new(m20240304_074901_subscription::Migration),
            Box::new(m20240410_082733_with_version_column_migration::Migration),
            Box::new(m20240410_154406_session_params::Migration),
            Box::new(m20240417_062305_subscription_internal_table_name::Migration),
            Box::new(m20240418_142249_function_runtime::Migration),
            Box::new(m20240506_112555_subscription_partial_ckpt::Migration),
            Box::new(m20240525_090457_secret::Migration),
            Box::new(m20240618_072634_function_compressed_binary::Migration),
            Box::new(m20240617_070131_index_column_properties::Migration),
        ]
    }
}

#[macro_export]
macro_rules! assert_not_has_tables {
    ($manager:expr, $( $table:ident ),+) => {
        $(
            assert!(
                !$manager
                    .has_table($table::Table.to_string())
                    .await?,
                "Table `{}` already exists",
                $table::Table.to_string()
            );
        )+
    };
}

#[macro_export]
macro_rules! drop_tables {
    ($manager:expr, $( $table:ident ),+) => {
        $(
            $manager
                .drop_table(
                    sea_orm_migration::prelude::Table::drop()
                        .table($table::Table)
                        .if_exists()
                        .cascade()
                        .to_owned(),
                )
                .await?;
        )+
    };
}
