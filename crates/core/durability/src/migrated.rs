use crate::{
    Migration, Sql, StoreError,
    sql::{check_format, migrate_format},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Migrated {
    component: &'static str,
    version: u32,
}

fn known(migrations: &[Migration]) -> u32 {
    let version = u32::try_from(migrations.len()).expect("a component's migrations count in u32");
    assert!(version > 0, "a component has at least one migration");
    version
}

impl Migrated {
    pub async fn migrate<H: Sql + ?Sized>(
        host: &H,
        component: &'static str,
        migrations: &[Migration],
    ) -> Result<Self, StoreError> {
        let version = known(migrations);
        migrate_format(host, component, migrations).await?;
        Ok(Self { component, version })
    }

    pub async fn check<H: Sql + ?Sized>(
        host: &H,
        component: &'static str,
        migrations: &[Migration],
    ) -> Result<Self, StoreError> {
        let version = known(migrations);
        check_format(host, component, version).await?;
        Ok(Self { component, version })
    }

    #[must_use]
    pub const fn component(&self) -> &'static str {
        self.component
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    pub fn assert_opens(&self, component: &'static str, migrations: &[Migration]) {
        assert_eq!(
            self.component, component,
            "a store opens with the Migrated of its own component"
        );
        assert_eq!(
            self.version,
            known(migrations),
            "a store opens with the Migrated of its current format"
        );
    }
}
